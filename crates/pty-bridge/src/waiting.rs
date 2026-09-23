//! Read waiting policy: return on new output, quiet output, a pattern in new output,
//! exit, a full read, cancellation or the deadline. A match never implies readiness.
use crate::mcp::ReadMode;
use pty_core::{PtyReader, Session, SessionState, render::render_text};
use regex::Regex;
use serde::Serialize;
use std::{
    collections::HashSet,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

/// Quiet period that ends a pattern wait when the pattern does not appear.
pub const UNTIL_IDLE_FALLBACK: Duration = Duration::from_secs(5);
/// Deadline used when a wait condition is given without `yield_time_ms`.
pub const CONDITION_TIMEOUT: Duration = Duration::from_secs(10);
/// Minimum spacing between evaluations while output streams.
const EVALUATION_INTERVAL: Duration = Duration::from_millis(25);
/// How far before `cursor` to look for the start of its line.
const LINE_LOOKBACK: u64 = 4096;

pub struct WaitOptions {
    pub timeout: Duration,
    pub idle: Option<Duration>,
    pub until: Option<Regex>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitReason {
    Output,
    Idle,
    Matched,
    Exited,
    Limit,
    Timeout,
    Cancelled,
}

#[derive(Debug, Clone, Serialize)]
pub struct WaitOutcome {
    pub reason: WaitReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched: Option<String>,
    pub waited_ms: u64,
}

pub async fn wait(
    session: &Session,
    cursor: u64,
    max: usize,
    mode: ReadMode,
    options: &WaitOptions,
    cancel: &CancellationToken,
) -> WaitOutcome {
    let started = Instant::now();
    let deadline = started + options.timeout;
    let reader = session.reader();
    let mut changes = session.subscribe();
    // Screen matches visible while nothing is unread were already seen and are not new.
    let baseline = options.until.as_ref().map(|re| {
        let screen = reader.screen();
        if cursor >= screen.end_cursor {
            screen_matches(re, &screen.lines)
        } else {
            HashSet::new()
        }
    });
    let done = |reason, matched| WaitOutcome {
        reason,
        matched,
        waited_ms: started.elapsed().as_millis() as u64,
    };
    loop {
        let snapshot = session.snapshot();
        let new_output = snapshot.retained_end > cursor;
        let mut wake = deadline;
        if new_output {
            let screen = reader.screen();
            let screen_mode = match mode {
                ReadMode::Screen => true,
                ReadMode::Auto => screen.alternate_screen,
                ReadMode::Text | ReadMode::Raw => false,
            };
            if let Some(re) = &options.until {
                let found = if screen_mode {
                    let seen = baseline.as_ref().expect("baseline exists with a pattern");
                    screen_matches(re, &screen.lines)
                        .into_iter()
                        .find(|m| !seen.contains(m))
                        .map(|(_, text)| text)
                } else {
                    text_match(&reader, cursor, max, snapshot.rows, snapshot.cols, re)
                };
                if found.is_some() {
                    return done(WaitReason::Matched, found);
                }
            }
            if snapshot.state != SessionState::Finished {
                if options.until.is_none() && options.idle.is_none() {
                    return done(WaitReason::Output, None);
                }
                if !screen_mode && snapshot.retained_end - cursor >= max as u64 {
                    return done(WaitReason::Limit, None);
                }
                if let (Some(idle), Some(last)) = (options.idle, snapshot.activity.last_output_at) {
                    let quiet_at = last + idle;
                    if Instant::now() >= quiet_at {
                        return done(WaitReason::Idle, None);
                    }
                    wake = wake.min(quiet_at);
                }
            }
        }
        if snapshot.state == SessionState::Finished {
            return done(WaitReason::Exited, None);
        }
        if Instant::now() >= deadline {
            return done(WaitReason::Timeout, None);
        }
        tokio::select! {
            _ = cancel.cancelled() => return done(WaitReason::Cancelled, None),
            changed = changes.changed() => {
                if changed.is_err() {
                    return done(WaitReason::Exited, None);
                }
                // Batch streaming output instead of rendering after every chunk.
                let batch = (Instant::now() + EVALUATION_INTERVAL).min(deadline);
                tokio::select! {
                    _ = cancel.cancelled() => return done(WaitReason::Cancelled, None),
                    _ = tokio::time::sleep_until(batch.into()) => {}
                }
            }
            _ = tokio::time::sleep_until(wake.into()) => {}
        }
    }
}

/// Matches rendered text from the start of the line containing `cursor`, so a prompt
/// split across reads still matches, but only matches ending in new output count.
fn text_match(
    reader: &PtyReader,
    cursor: u64,
    max: usize,
    rows: u16,
    cols: u16,
    re: &Regex,
) -> Option<String> {
    let before = reader.read(
        cursor.saturating_sub(LINE_LOOKBACK),
        LINE_LOOKBACK.min(cursor) as usize,
    );
    let anchor = before
        .bytes
        .iter()
        .rposition(|b| *b == b'\n')
        .map_or(before.start_cursor, |i| before.start_cursor + i as u64 + 1);
    let data = reader.read(anchor, (cursor.saturating_sub(anchor)) as usize + max);
    let prefix = (cursor.saturating_sub(data.start_cursor) as usize).min(data.bytes.len());
    let (text, _) = render_text(&data.bytes, rows, cols);
    let (seen, _) = render_text(&data.bytes[..prefix], rows, cols);
    new_match(re, &text, &seen).or_else(|| new_match(re, &trim_lines(&text), &trim_lines(&seen)))
}

fn new_match(re: &Regex, text: &str, seen: &str) -> Option<String> {
    let min_end = if text.starts_with(seen) {
        seen.len()
    } else {
        0
    };
    re.find_iter(text)
        .find(|m| m.end() > min_end)
        .map(|m| m.as_str().to_string())
}

/// The cursor line keeps its trailing spaces; patterns may be written with or without them.
fn trim_lines(text: &str) -> String {
    text.lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

fn screen_matches(re: &Regex, lines: &[String]) -> HashSet<(usize, String)> {
    lines
        .iter()
        .enumerate()
        .flat_map(|(row, line)| [line.as_str(), line.trim_end()].map(|line| (row, line)))
        .flat_map(|(row, line)| {
            re.find_iter(line)
                .map(move |m| (row, m.as_str().to_string()))
        })
        .collect()
}
