//! Where the server's time goes: `server --profile-ticks` adds up the time of every system,
//! schedule and command flush and logs the most expensive ones with each soak report (see
//! [`crate::soak`]), per simulation tick.
//!
//! The timings come from Bevy's tracing spans, which only exist in builds with Bevy's `trace`
//! feature: build the server with `--features profile` (a separate set of Bevy artifacts; the
//! default build stays untouched). Without it only the soak's tick and frame times are there.
//!
//! Systems run in parallel, so their times add up to more than the tick; schedules (`sched`)
//! and exclusive systems that run schedules include the systems inside them.

use std::{
    fmt::Write as _,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Instant,
};

use bevy::{
    log::{
        BoxedLayer,
        tracing::{
            Subscriber,
            field::{Field, Visit},
            span,
        },
        tracing_subscriber::{Layer, layer::Context, registry::LookupSpan},
    },
    prelude::*,
};

/// Distinct span names we keep time for; more are ignored.
const MAX_SLOTS: usize = 4096;

struct Slot {
    total_ns: AtomicU64,
    calls: AtomicU64,
    max_ns: AtomicU64,
}

impl Slot {
    const fn new() -> Self {
        Self {
            total_ns: AtomicU64::new(0),
            calls: AtomicU64::new(0),
            max_ns: AtomicU64::new(0),
        }
    }
}

static ENABLED: AtomicBool = AtomicBool::new(false);
static SLOTS: [Slot; MAX_SLOTS] = [const { Slot::new() }; MAX_SLOTS];
static NAMES: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Whether `--profile-ticks` installed the layer.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// For [`LogPlugin::custom_layer`](bevy::log::LogPlugin::custom_layer).
pub fn layer(_app: &mut App) -> Option<BoxedLayer> {
    ENABLED.store(true, Ordering::Relaxed);
    if cfg!(not(feature = "profile")) {
        eprintln!(
            "--profile-ticks: this build has no system timings (build with `--features profile`); \
             only tick and frame times are reported"
        );
    }
    Some(Box::new(ProfileLayer))
}

/// The slot a span's time goes to.
struct SlotId(usize);
/// When the span was entered.
struct Entered(Instant);

struct ProfileLayer;

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for ProfileLayer {
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        let span_name = attrs.metadata().name();
        let kind = match span_name {
            "system" => "",
            "schedule" => "sched ",
            "system_commands" => "cmds ",
            // Other spans (render passes, a camera's schedule, queue submits): by span name
            // plus its `name` or `camera` field, if any.
            _ if cfg!(feature = "profile") => "span ",
            _ => return,
        };
        let mut name = NameVisitor(None);
        attrs.record(&mut name);
        let name = match (kind, name.0) {
            ("span ", Some(field)) => format!("{span_name} {field}"),
            ("span ", None) => span_name.to_string(),
            (_, Some(name)) => short_name(&name),
            (_, None) => return,
        };
        // The client's own server runs on a thread of its own (`embedded`): its spans apart.
        let thread = if std::thread::current().name() == Some("Server") { "srv " } else { "" };
        let Some(slot) = intern(format!("{thread}{kind}{name}")) else {
            return;
        };
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(SlotId(slot));
        }
    }

    fn on_enter(&self, id: &span::Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            let mut extensions = span.extensions_mut();
            if extensions.get_mut::<SlotId>().is_some() {
                extensions.replace(Entered(Instant::now()));
            }
        }
    }

    fn on_exit(&self, id: &span::Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else {
            return;
        };
        let mut extensions = span.extensions_mut();
        let Some(Entered(started)) = extensions.remove::<Entered>() else {
            return;
        };
        let Some(SlotId(slot)) = extensions.get_mut::<SlotId>().map(|s| SlotId(s.0)) else {
            return;
        };
        let ns = started.elapsed().as_nanos() as u64;
        let slot = &SLOTS[slot];
        slot.total_ns.fetch_add(ns, Ordering::Relaxed);
        slot.calls.fetch_add(1, Ordering::Relaxed);
        slot.max_ns.fetch_max(ns, Ordering::Relaxed);
    }
}

struct NameVisitor(Option<String>);

impl Visit for NameVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "name" || field.name() == "camera" {
            self.0 = Some(value.to_string());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "name" || field.name() == "camera" {
            self.0 = Some(format!("{value:?}").trim_matches('"').to_string());
        }
    }
}

fn intern(name: String) -> Option<usize> {
    let mut names = NAMES.lock().ok()?;
    if let Some(index) = names.iter().position(|n| *n == name) {
        return Some(index);
    }
    (names.len() < MAX_SLOTS).then(|| {
        names.push(name);
        names.len() - 1
    })
}

/// `a::b::c::f<x::y::T>` -> `c::f<T>`: the last module and the item, generic arguments without
/// their paths.
fn short_name(name: &str) -> String {
    let (outer, generics) = match name.find('<') {
        Some(at) => name.split_at(at),
        None => (name, ""),
    };
    let segments: Vec<&str> = outer.split("::").collect();
    let mut short = segments[segments.len().saturating_sub(2)..].join("::");
    // Inside the generics keep only the last segment of every path.
    let mut word = String::new();
    for c in generics.chars() {
        if c.is_alphanumeric() || c == '_' || c == ':' {
            word.push(c);
        } else {
            short.push_str(word.rsplit("::").next().unwrap_or(""));
            word.clear();
            short.push(c);
        }
    }
    short.push_str(word.rsplit("::").next().unwrap_or(""));
    short
}

/// One line per span name (the `top` most expensive) with its time since the last call, per
/// simulation tick: average and share of the tick, calls per tick, the longest call. Resets
/// the counters.
pub fn take_report(ticks: u32, top: usize) -> String {
    let Ok(names) = NAMES.lock() else {
        return String::new();
    };
    let mut rows: Vec<(usize, u64, u64, u64)> = (0..names.len())
        .map(|i| {
            let slot = &SLOTS[i];
            (
                i,
                slot.total_ns.swap(0, Ordering::Relaxed),
                slot.calls.swap(0, Ordering::Relaxed),
                slot.max_ns.swap(0, Ordering::Relaxed),
            )
        })
        .filter(|row| row.2 > 0)
        .collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.1));
    let ticks = ticks.max(1) as f64;
    let mut out = String::new();
    if rows.is_empty() {
        out.push_str("  (no timings: build the server with `--features profile`)\n");
    }
    for (i, total, calls, max) in rows.into_iter().take(top) {
        let _ = writeln!(
            out,
            "  {:>7.3} ms/tick {:>6.2} calls/tick {:>7.1} ms max  {}",
            total as f64 / 1e6 / ticks,
            calls as f64 / ticks,
            max as f64 / 1e6,
            names[i]
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::short_name;

    #[test]
    fn shortens_names() {
        assert_eq!(short_name("game_server::bots::think"), "bots::think");
        assert_eq!(short_name("think"), "think");
        assert_eq!(
            short_name("avian3d::schedule::run_physics_schedule<avian3d::a::B, c::D>"),
            "schedule::run_physics_schedule<B, D>"
        );
    }
}
