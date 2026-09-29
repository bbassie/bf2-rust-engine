//! Voice chat like BF2's, done in a modern way: push to talk (or voice activation) on the
//! squad channel (B) and the command channel (H: squad leaders and the commander), Opus at
//! 24 kbps in 20 ms frames, relayed by the server to whoever may hear it (see
//! `game_shared::voice` for the rules, `game_server::voice` for the relay).
//!
//! - [`capture`]: the microphone (cpal), opened only while we transmit, or the test input.
//! - [`playback`]: our own output stream mixing every talker through a jitter buffer.
//! - [`ui`]: who is talking (and whether we are), and muting players on the scoreboard.
//! - `menu::voice`: the settings (Settings > Audio).
//!
//! Privacy: nothing is captured unless voice chat is on and push to talk is held, voice
//! activation is chosen, or the mic test runs on the settings page. Voice chat off also
//! stops playing others. Players muted on the scoreboard are dropped on arrival; players an
//! admin muted (`VoiceMuted`) can't talk at all.
//!
//! Testing without a microphone: `BF2_VOICE_TEST_INPUT=tone` (or `tone:<Hz>`, or a WAV
//! file's path) feeds that instead of the microphone, still gated by push to talk. The log
//! says `voice: talking on squad` / `voice: stopped talking on squad: N frames sent` for our
//! bursts and `voice: hearing <name> on <channel>` / `voice: <name> stopped on <channel>: N
//! frames received` for others', which scenarios check with `ExpectLog`/`ForbidLog`.

mod capture;
mod playback;
mod ui;

use std::collections::HashSet;

use bevy::prelude::*;
use game_shared::{
    commander::Commander,
    protocol::{Player, PlayerNetId, Team},
    squad::SquadMember,
    voice::{VoiceChannel, VoiceMuted, VoicePacket, VoiceRelay, VoiceRole, effective_channel, refusal},
};
use game_voice::{
    BITRATE,
    codec::Encoder,
    level::{VoiceGate, apply_gain, rms_db},
    test_input::TestInput,
};
use serde::{Deserialize, Serialize};

pub use self::ui::ScoreboardCursor;
use self::{capture::Capture, playback::Output};
use crate::{
    chat::ChatBox,
    menu::{Menu, Screen},
    net::{ActiveMatch, LocalPlayer},
    settings::{Action, Actions, Settings},
};

pub struct VoicePlugin;

impl Plugin for VoicePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<VoiceState>()
            .init_resource::<VoiceTestInput>()
            .init_resource::<MicTest>()
            .init_resource::<VoiceMutes>()
            .init_resource::<Talkers>()
            .init_resource::<VoiceDevices>()
            .init_resource::<ScoreboardCursor>()
            .insert_non_send(VoiceAudio::default())
            .add_systems(Startup, read_test_input)
            .add_systems(Update, (apply_output_settings, receive_voice, transmit, log_reach).chain())
            .add_plugins(ui::VoiceUiPlugin);
    }
}

/// Voice chat settings (`Settings::voice`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct VoiceSettings {
    /// Off: nothing is captured and nobody is played.
    pub enabled: bool,
    pub mode: VoiceMode,
    /// Voice activation opens at this level (dBFS).
    pub activation_threshold_db: f32,
    /// Multiplies the microphone, 0..4.
    pub input_gain: f32,
    /// Others' voices, on top of the master volume, 0..2.
    pub volume: f32,
    /// Microphone by name; `None` is the system's default.
    pub input_device: Option<String>,
    /// Where voices play, by name; `None` is the system's default.
    pub output_device: Option<String>,
}

impl Default for VoiceSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: VoiceMode::PushToTalk,
            activation_threshold_db: -42.0,
            input_gain: 1.0,
            volume: 1.0,
            input_device: None,
            output_device: None,
        }
    }
}

impl VoiceSettings {
    /// For `Settings::set` (scenarios): `voice_enabled`, `voice_mode` (`push` or
    /// `activation`), `voice_threshold`, `voice_gain`, `voice_volume`.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        let num = || value.parse::<f32>().map_err(|_| format!("setting {key}: expected a number, got {value}"));
        match key {
            "voice_enabled" | "voice" => {
                self.enabled = match value.to_ascii_lowercase().as_str() {
                    "on" | "true" | "1" | "yes" => true,
                    "off" | "false" | "0" | "no" => false,
                    _ => return Err(format!("setting {key}: expected on or off, got {value}")),
                }
            }
            "voice_mode" => {
                self.mode = VoiceMode::parse(value).ok_or_else(|| format!("setting {key}: expected push or activation"))?
            }
            "voice_threshold" => self.activation_threshold_db = num()?.clamp(-80.0, 0.0),
            "voice_gain" => self.input_gain = num()?.clamp(0.0, 4.0),
            "voice_volume" => self.volume = num()?.clamp(0.0, 2.0),
            _ => return Err(format!("unknown setting {key}")),
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum VoiceMode {
    /// Transmit while a voice key is held (BF2's way).
    #[default]
    PushToTalk,
    /// Transmit on the squad channel whenever the microphone hears speech louder than the
    /// threshold (push to talk still works, and is the only way onto the command channel for
    /// squad leaders).
    VoiceActivation,
}

impl VoiceMode {
    pub const ALL: [VoiceMode; 2] = [VoiceMode::PushToTalk, VoiceMode::VoiceActivation];

    pub fn label(self) -> &'static str {
        match self {
            VoiceMode::PushToTalk => "Push to talk",
            VoiceMode::VoiceActivation => "Voice activation",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "push" | "ptt" | "push_to_talk" | "pushtotalk" => Some(VoiceMode::PushToTalk),
            "activation" | "vad" | "voice_activation" | "voiceactivation" => Some(VoiceMode::VoiceActivation),
            _ => None,
        }
    }
}

/// Frames sent after the key is let go, so the last word isn't cut: 200 ms.
const TAIL_FRAMES: u32 = 10;
/// A pause this long ends someone's burst (for the HUD and the logs).
const BURST_GAP: f64 = 0.5;
/// The mixer's talker id for the mic test's loopback.
const LOOPBACK: u64 = u64::MAX;

/// The test input from `BF2_VOICE_TEST_INPUT`, replacing the microphone.
#[derive(Resource, Default)]
pub struct VoiceTestInput(pub Option<TestInput>);

/// Our side of voice chat, for the HUD and the settings page.
#[derive(Resource)]
pub struct VoiceState {
    /// A voice key is held: the channel it talks on.
    pub key: Option<VoiceChannel>,
    /// Frames are going out on this channel.
    pub sending: Option<VoiceChannel>,
    /// Why the held key doesn't transmit.
    pub refused: Option<&'static str>,
    /// Level of the last captured frame (after the gain), dBFS.
    pub level_db: f32,
    /// What the open capture is, if one is open.
    pub input: Option<String>,
    /// The last device problem.
    pub error: Option<String>,
    seq: u16,
    tail: u32,
    tail_channel: Option<VoiceChannel>,
    frames_sent: u32,
}

impl Default for VoiceState {
    fn default() -> Self {
        Self {
            key: None,
            sending: None,
            refused: None,
            level_db: game_voice::level::SILENCE_DB,
            input: None,
            error: None,
            seq: 0,
            tail: 0,
            tail_channel: None,
            frames_sent: 0,
        }
    }
}

/// The mic test on the settings page: capture with the level meter, and hear ourselves
/// through the codec.
#[derive(Resource, Default)]
pub struct MicTest {
    pub active: bool,
}

/// Players we muted, by name (lowercase), for the whole session.
#[derive(Resource, Default)]
pub struct VoiceMutes(pub HashSet<String>);

impl VoiceMutes {
    pub fn is_muted(&self, name: &str) -> bool {
        self.0.contains(&name.to_lowercase())
    }

    pub fn toggle(&mut self, name: &str) {
        let key = name.to_lowercase();
        if !self.0.remove(&key) {
            self.0.insert(key);
        }
    }
}

/// Someone we hear.
#[derive(Clone, Debug)]
pub struct Talker {
    pub id: PlayerNetId,
    pub name: String,
    pub channel: VoiceChannel,
    last: f64,
    frames: u32,
}

/// Who is talking to us now.
#[derive(Resource, Default)]
pub struct Talkers(pub Vec<Talker>);

/// Audio device names, for the settings page.
#[derive(Resource, Default)]
pub struct VoiceDevices {
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    /// When they were listed (real seconds).
    pub listed_at: Option<f64>,
}

impl VoiceDevices {
    pub fn refresh(&mut self, now: f64) {
        if self.listed_at.is_some_and(|at| now - at < 5.0) {
            return;
        }
        (self.inputs, self.outputs) = capture::list_devices();
        self.listed_at = Some(now);
    }
}

/// The audio streams and codec state (cpal streams stay on the main thread).
#[derive(Default)]
struct VoiceAudio {
    capture: Option<Capture>,
    encoder: Option<Encoder>,
    gate: Option<VoiceGate>,
    /// Sequence numbers of the mic test's own frames.
    loopback_seq: u16,
    output: Option<Output>,
    /// The output failed to open; tried again when the settings change.
    output_failed: bool,
    /// The capture failed to open for this key press; tried again on the next.
    capture_failed: bool,
    gain: f32,
}

impl VoiceAudio {
    fn output(&mut self, settings: &Settings) -> Option<&Output> {
        if self.output.is_none() && !self.output_failed {
            match Output::open(settings.voice.output_device.as_deref()) {
                Ok(output) => {
                    info!("voice: playing on {}", output.description);
                    self.gain = settings.master_volume * settings.voice.volume;
                    output.set_gain(self.gain);
                    self.output = Some(output);
                }
                Err(err) => {
                    warn!("voice: can't open the audio output: {err}");
                    self.output_failed = true;
                }
            }
        }
        self.output.as_ref()
    }
}

fn read_test_input(mut commands: Commands) {
    let input = std::env::var("BF2_VOICE_TEST_INPUT").ok().filter(|v| !v.trim().is_empty()).and_then(|spec| {
        match TestInput::parse(&spec) {
            Ok(input) => {
                info!("voice: BF2_VOICE_TEST_INPUT={spec} replaces the microphone");
                Some(input)
            }
            Err(err) => {
                warn!("voice: BF2_VOICE_TEST_INPUT: {err}");
                None
            }
        }
    });
    commands.insert_resource(VoiceTestInput(input));
}

/// Volume and output device changes.
fn apply_output_settings(settings: Res<Settings>, mut audio: NonSendMut<VoiceAudio>, mut device: Local<Option<Option<String>>>) {
    if !settings.is_changed() {
        return;
    }
    let wanted = settings.voice.output_device.clone();
    if device.as_ref().is_some_and(|d| *d != wanted) {
        // Another device: opened again when the next voice arrives.
        audio.output = None;
    }
    *device = Some(wanted);
    audio.output_failed = false;
    let gain = settings.master_volume * settings.voice.volume;
    if (gain - audio.gain).abs() > 1e-4 {
        audio.gain = gain;
        if let Some(output) = &audio.output {
            output.set_gain(gain);
        }
    }
}

/// Others' voice frames: to the mixer, unless muted; notes who talks.
#[allow(clippy::too_many_arguments)]
fn receive_voice(
    time: Res<Time<Real>>,
    settings: Res<Settings>,
    mutes: Res<VoiceMutes>,
    mut relays: MessageReader<VoiceRelay>,
    players: Query<(&Player, &PlayerNetId)>,
    mut talkers: ResMut<Talkers>,
    mut audio: NonSendMut<VoiceAudio>,
) {
    let now = time.elapsed_secs_f64();
    for relay in relays.read() {
        if !settings.voice.enabled {
            continue;
        }
        let name = players
            .iter()
            .find(|(_, id)| **id == relay.talker)
            .map_or_else(|| format!("player {}", relay.talker.0), |(p, _)| p.name.clone());
        if mutes.is_muted(&name) {
            if let Some(output) = &audio.output {
                output.forget(relay.talker.0);
            }
            continue;
        }
        match talkers.0.iter_mut().find(|t| t.id == relay.talker && t.channel == relay.channel) {
            Some(talker) => {
                talker.last = now;
                talker.frames += 1;
            }
            None => {
                info!("voice: hearing {name} on {}", relay.channel.label());
                talkers.0.push(Talker { id: relay.talker, name, channel: relay.channel, last: now, frames: 1 });
            }
        }
        if let Some(output) = audio.output(&settings) {
            output.push(relay.talker.0, relay.seq, relay.data.clone());
        }
    }
    let output = audio.output.as_ref();
    talkers.0.retain(|talker| {
        let talking = now - talker.last <= BURST_GAP;
        if !talking {
            // The jitter buffer's counts are for the whole session with this talker.
            let played = output.and_then(|o| o.stats(talker.id.0)).map_or_else(String::new, |s| {
                format!(
                    " (so far {} played, {} recovered, {} concealed, {} late, {} dropped)",
                    s.played, s.recovered, s.concealed, s.late, s.dropped
                )
            });
            info!("voice: {} stopped on {}: {} frames received{played}", talker.name, talker.channel.label(), talker.frames);
        }
        talking
    });
}

/// Push to talk, voice activation and the mic test: opens and closes the capture, encodes
/// and sends our frames.
#[allow(clippy::too_many_arguments)]
fn transmit(
    settings: Res<Settings>,
    actions: Actions,
    screen: Res<State<Screen>>,
    menu: Res<Menu>,
    chat: Res<ChatBox>,
    active: Res<ActiveMatch>,
    test_input: Res<VoiceTestInput>,
    mic_test: Res<MicTest>,
    me: Query<(&Team, Option<&SquadMember>, Has<Commander>, Has<VoiceMuted>), With<LocalPlayer>>,
    mut state: ResMut<VoiceState>,
    mut packets: MessageWriter<VoicePacket>,
    mut audio: NonSendMut<VoiceAudio>,
) {
    let voice = &settings.voice;
    let playing = *screen.get() == Screen::InGame && !menu.paused && chat.typing.is_none() && active.setup.is_some();
    let role = me.single().ok().map(|(team, squad, commander, muted)| {
        (VoiceRole { team: *team, squad: squad.copied(), commander, bot: false }, muted)
    });

    // Which key, and may we?
    let requested = if !playing || !voice.enabled {
        None
    } else if actions.pressed(Action::VoiceCommand) {
        Some(VoiceChannel::Command)
    } else if actions.pressed(Action::VoiceSquad) {
        Some(VoiceChannel::Squad)
    } else {
        None
    };
    let (key, refused) = match (requested, role) {
        (Some(channel), Some((role, muted))) => {
            let channel = effective_channel(channel, &role);
            (Some(channel), if muted { Some("muted by an admin") } else { refusal(channel, &role) })
        }
        (Some(channel), None) => (Some(channel), Some("not playing")),
        (None, _) => (None, None),
    };
    if key.is_none() {
        audio.capture_failed = false;
    }
    state.key = key;
    state.refused = refused;
    let push_channel = key.filter(|_| refused.is_none());
    if let Some(channel) = push_channel {
        state.tail = TAIL_FRAMES;
        state.tail_channel = Some(channel);
    } else if !playing || !voice.enabled {
        state.tail = 0;
    }
    // Voice activation talks to the squad (a squadless commander: to his leaders).
    let activation_channel = match role {
        Some((role, false)) if playing && voice.enabled && voice.mode == VoiceMode::VoiceActivation => {
            let channel = effective_channel(VoiceChannel::Squad, &role);
            refusal(channel, &role).is_none().then_some(channel)
        }
        _ => None,
    };
    let testing = mic_test.active && voice.enabled;

    // Open or close the capture.
    let wanted = push_channel.is_some() || state.tail > 0 || activation_channel.is_some() || testing;
    if !wanted {
        if audio.capture.take().is_some() {
            info!("voice: microphone closed");
            state.input = None;
        }
        audio.gate = None;
        finish_burst(&mut state);
        return;
    }
    if audio.capture.is_none() && !audio.capture_failed {
        match Capture::open(voice.input_device.as_deref(), test_input.0.as_ref()) {
            Ok(capture) => {
                info!("voice: microphone open: {}", capture.description);
                state.input = Some(capture.description.clone());
                state.error = None;
                audio.capture = Some(capture);
                audio.encoder = None;
            }
            Err(err) => {
                warn!("voice: can't open the microphone: {err}");
                state.error = Some(err);
                audio.capture_failed = true;
                return;
            }
        }
    }
    if audio.encoder.is_none() {
        match Encoder::new(BITRATE) {
            Ok(encoder) => audio.encoder = Some(encoder),
            Err(err) => {
                warn!("voice: {err}");
                return;
            }
        }
    }
    let threshold = voice.activation_threshold_db;
    let gate = audio.gate.get_or_insert_with(|| VoiceGate::new(threshold));
    gate.threshold_db = threshold;

    let frames = audio.capture.as_mut().map(Capture::frames).unwrap_or_default();
    for mut frame in frames {
        apply_gain(&mut frame, voice.input_gain);
        let level = rms_db(&frame);
        state.level_db = level;
        let channel = if let Some(channel) = push_channel {
            Some(channel)
        } else if state.tail > 0 {
            state.tail -= 1;
            state.tail_channel
        } else if let Some(channel) = activation_channel {
            audio.gate.as_mut().is_some_and(|g| g.update(level)).then_some(channel)
        } else {
            None
        };
        if channel.is_none() && !testing {
            finish_burst(&mut state);
            continue;
        }
        let Some(Ok(packet)) = audio.encoder.as_mut().map(|e| e.encode(&frame)) else {
            continue;
        };
        if testing {
            // Hear ourselves through the codec (the mixer decodes it like anyone's).
            let seq = audio.loopback_seq;
            audio.loopback_seq = seq.wrapping_add(1);
            if let Some(output) = audio.output(&settings) {
                output.push(LOOPBACK, seq, packet.clone());
            }
        }
        let Some(channel) = channel else {
            continue;
        };
        if state.sending != Some(channel) {
            finish_burst(&mut state);
            info!(
                "voice: talking on {}{}",
                channel.label(),
                if test_input.0.is_some() { " (test input)" } else { "" }
            );
            state.sending = Some(channel);
        }
        packets.write(VoicePacket { channel, seq: state.seq, data: packet });
        state.seq = state.seq.wrapping_add(1);
        state.frames_sent += 1;
    }
}

/// Logs the end of our burst, if one is going.
fn finish_burst(state: &mut VoiceState) {
    if let Some(channel) = state.sending.take() {
        info!("voice: stopped talking on {}: {} frames sent", channel.label(), state.frames_sent);
        state.frames_sent = 0;
    }
}

/// Logs whom our channels reach whenever that changes (`voice: squad channel reaches Bob`),
/// and when an admin mutes or unmutes us: for players reading the log, and for scenarios to
/// wait on before talking.
#[allow(clippy::type_complexity)]
fn log_reach(
    time: Res<Time<Real>>,
    players: Query<(&Player, &Team, Option<&SquadMember>, Has<Commander>, Has<VoiceMuted>, Has<LocalPlayer>)>,
    mut last: Local<(f64, Option<[String; 2]>, bool)>,
) {
    let now = time.elapsed_secs_f64();
    if now - last.0 < 0.5 {
        return;
    }
    last.0 = now;
    let role_of = |player: &Player, team: &Team, squad: Option<&SquadMember>, commander: bool| VoiceRole {
        team: *team,
        squad: squad.copied(),
        commander,
        bot: player.is_bot,
    };
    let Some((me, muted)) = players
        .iter()
        .find(|p| p.5)
        .map(|(player, team, squad, commander, muted, _)| (role_of(player, team, squad, commander), muted))
    else {
        *last = (now, None, false);
        return;
    };
    let reach = [VoiceChannel::Squad, VoiceChannel::Command].map(|channel| {
        if refusal(channel, &me).is_some() {
            return format!("{} channel: can't talk ({})", channel.label(), refusal(channel, &me).unwrap_or_default());
        }
        let mut names: Vec<&str> = players
            .iter()
            .filter(|p| !p.5 && game_shared::voice::hears(channel, &me, &role_of(p.0, p.1, p.2, p.3)))
            .map(|p| p.0.name.as_str())
            .collect();
        names.sort_unstable();
        let names = if names.is_empty() { "nobody".to_string() } else { names.join(", ") };
        format!("{} channel reaches {names}", channel.label())
    });
    let previous = last.1.clone().unwrap_or_default();
    for (line, old) in reach.iter().zip(previous.iter()) {
        if line != old {
            info!("voice: {line}");
        }
    }
    if muted != last.2 {
        info!("voice: an admin {} you", if muted { "muted" } else { "unmuted" });
    }
    last.1 = Some(reach);
    last.2 = muted;
}
