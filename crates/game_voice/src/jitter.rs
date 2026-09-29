//! One talker's jitter buffer: packets arrive late, early, twice or never; the speaker needs
//! one frame every 20 ms.
//!
//! Packets carry a 16-bit sequence number (wrapping). The buffer holds them in order and
//! starts playing once [`JitterBuffer::target`] frames are in (or the oldest has waited that
//! long, for short bursts). Each [`JitterBuffer::next`] (called once per 20 ms by the mixer)
//! says what to play: the next packet, a lost one recovered from its successor's FEC, a
//! concealed frame, or nothing. After a few frames with nothing left the talker has stopped
//! (the end of a push-to-talk burst) and the buffer waits to fill again.
//!
//! The delay adapts: a packet that arrives after its turn raises the target by a frame (up to
//! [`MAX_TARGET`]); a buffer running well over the target skips its oldest frame so the delay
//! comes back down.

use std::collections::BTreeMap;

/// Frames buffered before playing starts, at first: 60 ms.
pub const START_TARGET: usize = 3;
/// Most frames the target grows to after late packets: 160 ms.
pub const MAX_TARGET: usize = 8;
/// Frames concealed in a row before the talker counts as stopped.
pub const MAX_CONCEALED: u32 = 4;
/// Packets kept at most (a flood from one talker can't grow the buffer).
pub const CAPACITY: usize = 32;

/// What to play for the next 20 ms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Playout {
    /// Decode this packet.
    Packet(Vec<u8>),
    /// The packet is lost but its successor is here: decode the successor's FEC data (the
    /// successor itself plays next).
    Recover(Vec<u8>),
    /// Lost: conceal it.
    Conceal,
    /// Not talking (or still buffering): silence.
    Silence,
}

#[derive(Debug)]
pub struct JitterBuffer {
    /// By extended (unwrapped) sequence number.
    packets: BTreeMap<i64, Vec<u8>>,
    /// Next extended sequence number to play; `None` while waiting to start.
    playing: Option<i64>,
    /// Highest extended sequence number seen, for unwrapping.
    highest: Option<i64>,
    /// Frames to buffer before starting.
    pub target: usize,
    /// Calls of `next` since the oldest waiting packet arrived, while not playing.
    waited: usize,
    concealed: u32,
    /// Counters, for logs and tests.
    pub stats: JitterStats,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JitterStats {
    pub received: u32,
    pub played: u32,
    pub recovered: u32,
    pub concealed: u32,
    /// Arrived after their turn (or twice).
    pub late: u32,
    /// Skipped to bring the delay down, or dropped as over capacity.
    pub dropped: u32,
}

impl Default for JitterBuffer {
    fn default() -> Self {
        Self {
            packets: BTreeMap::new(),
            playing: None,
            highest: None,
            target: START_TARGET,
            waited: 0,
            concealed: 0,
            stats: JitterStats::default(),
        }
    }
}

impl JitterBuffer {
    /// The sequence number on its unwrapped scale, nearest the highest seen.
    fn unwrap(&self, seq: u16) -> i64 {
        match self.highest {
            None => i64::from(seq),
            Some(highest) => {
                let delta = seq.wrapping_sub(highest as u16) as i16;
                highest + i64::from(delta)
            }
        }
    }

    /// A packet arrived.
    pub fn push(&mut self, seq: u16, packet: Vec<u8>) {
        let seq = self.unwrap(seq);
        self.stats.received += 1;
        if self.playing.is_some_and(|next| seq < next) || self.packets.contains_key(&seq) {
            self.stats.late += 1;
            // Only a packet that just missed its turn says the delay is too short; an old
            // duplicate or a stray from long ago doesn't.
            if self.playing.is_some_and(|next| next - seq <= 2) && self.target < MAX_TARGET {
                self.target += 1;
            }
            return;
        }
        if self.packets.is_empty() && self.playing.is_none() {
            self.waited = 0;
        }
        self.highest = Some(self.highest.map_or(seq, |h| h.max(seq)));
        self.packets.insert(seq, packet);
        while self.packets.len() > CAPACITY {
            self.packets.pop_first();
            self.stats.dropped += 1;
        }
    }

    /// Whether anything is playing or waiting to.
    pub fn is_active(&self) -> bool {
        self.playing.is_some() || !self.packets.is_empty()
    }

    /// Frames waiting.
    pub fn buffered(&self) -> usize {
        self.packets.len()
    }

    /// What to play for the next frame.
    pub fn next(&mut self) -> Playout {
        let Some(next) = self.playing else {
            let Some((&first, _)) = self.packets.first_key_value() else {
                return Playout::Silence;
            };
            self.waited += 1;
            // Start once the target is buffered, or once the first packet waited as long as
            // that would take (a burst shorter than the target still plays).
            if self.packets.len() < self.target && self.waited < self.target {
                return Playout::Silence;
            }
            self.playing = Some(first);
            self.concealed = 0;
            return self.next();
        };
        // Running well over the target: skip the oldest frame to bring the delay down.
        if self.packets.len() > self.target + 3
            && let Some((&first, _)) = self.packets.first_key_value()
            && first >= next
        {
            self.packets.pop_first();
            self.stats.dropped += 1;
            self.playing = Some(first + 1);
            return self.next();
        }
        if let Some(packet) = self.packets.remove(&next) {
            self.playing = Some(next + 1);
            self.concealed = 0;
            self.stats.played += 1;
            return Playout::Packet(packet);
        }
        let Some((&first, _)) = self.packets.first_key_value() else {
            // Nothing left: conceal a little, then the talker has stopped.
            self.concealed += 1;
            if self.concealed > MAX_CONCEALED {
                self.playing = None;
                self.concealed = 0;
                return Playout::Silence;
            }
            self.playing = Some(next + 1);
            self.stats.concealed += 1;
            return Playout::Conceal;
        };
        if first > next + 2 * MAX_TARGET as i64 {
            // A long gap (packets lost for a while, or a new burst): start over from here.
            self.playing = Some(first);
            return self.next();
        }
        self.playing = Some(next + 1);
        if first == next + 1 {
            self.stats.recovered += 1;
            Playout::Recover(self.packets[&first].clone())
        } else {
            self.stats.concealed += 1;
            Playout::Conceal
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(seq: u16) -> Vec<u8> {
        seq.to_le_bytes().to_vec()
    }

    fn played(playout: &Playout) -> Option<u16> {
        match playout {
            Playout::Packet(p) => Some(u16::from_le_bytes([p[0], p[1]])),
            _ => None,
        }
    }

    #[test]
    fn waits_for_the_target_then_plays_in_order() {
        let mut buffer = JitterBuffer::default();
        buffer.push(11, packet(11));
        buffer.push(10, packet(10));
        assert_eq!(buffer.next(), Playout::Silence);
        buffer.push(12, packet(12));
        assert_eq!(played(&buffer.next()), Some(10));
        assert_eq!(played(&buffer.next()), Some(11));
        assert_eq!(played(&buffer.next()), Some(12));
    }

    #[test]
    fn a_short_burst_still_plays() {
        let mut buffer = JitterBuffer::default();
        buffer.push(5, packet(5));
        let first: Vec<Playout> = (0..START_TARGET + 1).map(|_| buffer.next()).collect();
        assert!(first.iter().any(|p| played(p) == Some(5)), "{first:?}");
    }

    #[test]
    fn a_lost_packet_is_recovered_from_its_successor() {
        let mut buffer = JitterBuffer::default();
        for seq in [0, 1, 3, 4] {
            buffer.push(seq, packet(seq));
        }
        assert_eq!(played(&buffer.next()), Some(0));
        assert_eq!(played(&buffer.next()), Some(1));
        assert_eq!(buffer.next(), Playout::Recover(packet(3)));
        assert_eq!(played(&buffer.next()), Some(3));
        assert_eq!(played(&buffer.next()), Some(4));
        assert_eq!(buffer.stats.recovered, 1);
    }

    #[test]
    fn two_lost_packets_are_concealed_then_recovered() {
        let mut buffer = JitterBuffer::default();
        for seq in [0, 1, 2, 5] {
            buffer.push(seq, packet(seq));
        }
        for seq in 0..3 {
            assert_eq!(played(&buffer.next()), Some(seq));
        }
        assert_eq!(buffer.next(), Playout::Conceal);
        assert_eq!(buffer.next(), Playout::Recover(packet(5)));
        assert_eq!(played(&buffer.next()), Some(5));
    }

    #[test]
    fn a_talker_stops_after_a_few_concealed_frames() {
        let mut buffer = JitterBuffer::default();
        for seq in 0..3 {
            buffer.push(seq, packet(seq));
        }
        for _ in 0..3 {
            assert!(played(&buffer.next()).is_some());
        }
        for _ in 0..MAX_CONCEALED {
            assert_eq!(buffer.next(), Playout::Conceal);
        }
        assert_eq!(buffer.next(), Playout::Silence);
        assert!(!buffer.is_active());
        // The next burst starts over, buffering first.
        buffer.push(40, packet(40));
        assert_eq!(buffer.next(), Playout::Silence);
    }

    #[test]
    fn late_packets_are_dropped_and_raise_the_target() {
        let mut buffer = JitterBuffer::default();
        for seq in [0, 1, 2, 4] {
            buffer.push(seq, packet(seq));
        }
        for _ in 0..3 {
            buffer.next();
        }
        // 3 is recovered from 4's FEC, then 3 itself shows up: too late.
        assert_eq!(buffer.next(), Playout::Recover(packet(4)));
        buffer.push(3, packet(3));
        assert_eq!(buffer.stats.late, 1);
        assert_eq!(buffer.target, START_TARGET + 1);
        assert_eq!(played(&buffer.next()), Some(4));
        // Duplicates are late too.
        buffer.push(4, packet(4));
        assert_eq!(buffer.stats.late, 2);
    }

    #[test]
    fn sequence_numbers_wrap() {
        let mut buffer = JitterBuffer::default();
        for seq in [65534u16, 65535, 0, 1] {
            buffer.push(seq, packet(seq));
        }
        let order: Vec<Option<u16>> = (0..4).map(|_| played(&buffer.next())).collect();
        assert_eq!(order, vec![Some(65534), Some(65535), Some(0), Some(1)]);
    }

    #[test]
    fn a_backlog_is_skipped_down_to_the_target() {
        let mut buffer = JitterBuffer::default();
        for seq in 0..20 {
            buffer.push(seq, packet(seq));
        }
        // Plays, skipping ahead until the buffer is near the target again.
        let mut last = None;
        for _ in 0..6 {
            if let Some(seq) = played(&buffer.next()) {
                last = Some(seq);
            }
        }
        assert!(buffer.buffered() <= buffer.target + 3, "{}", buffer.buffered());
        assert!(last.unwrap() > 6, "{last:?}");
        assert!(buffer.stats.dropped > 0);
    }

    #[test]
    fn capacity_is_bounded() {
        let mut buffer = JitterBuffer::default();
        for seq in 0..(CAPACITY as u16 * 3) {
            buffer.push(seq, packet(seq));
        }
        assert_eq!(buffer.buffered(), CAPACITY);
    }
}
