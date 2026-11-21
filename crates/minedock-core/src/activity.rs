//! Pure Vanilla log parsing and player-activity projection.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const PLAYER_ACTIVITY_SCHEMA_VERSION: u16 = 2;
const LEGACY_PLAYER_ACTIVITY_SCHEMA_VERSION: u16 = 1;
pub const MAX_PLAYER_NAME_BYTES: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VanillaLogEvent {
    ServerReady,
    PlayerJoined { name: String },
    PlayerLeft { name: String },
    PlayerDied { name: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PresenceBaseline {
    #[default]
    Unknown,
    Known,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlayerActivity {
    pub name: String,
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub total_playtime_seconds: u64,
    pub deaths: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivePlayerJoin {
    pub name: String,
    pub joined_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlayerActivitySnapshot {
    pub schema_version: u16,
    pub baseline: PresenceBaseline,
    pub current_players: Vec<String>,
    /// The start of each currently open play interval. This is persisted so
    /// a join in one output poll can be paired with a leave in a later poll.
    #[serde(default)]
    pub current_player_joined_at: Vec<ActivePlayerJoin>,
    pub players: Vec<PlayerActivity>,
    pub last_event_at: Option<DateTime<Utc>>,
}

impl Default for PlayerActivitySnapshot {
    fn default() -> Self {
        Self {
            schema_version: PLAYER_ACTIVITY_SCHEMA_VERSION,
            baseline: PresenceBaseline::Unknown,
            current_players: Vec::new(),
            current_player_joined_at: Vec::new(),
            players: Vec::new(),
            last_event_at: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerActivitySummary {
    pub baseline: PresenceBaseline,
    pub current_players: Vec<String>,
    pub known_players: Vec<String>,
    pub total_playtime_seconds: u64,
}

#[derive(Debug, Default, Clone)]
pub struct PlayerActivityTracker {
    baseline: PresenceBaseline,
    current: BTreeSet<String>,
    players: BTreeMap<String, PlayerActivity>,
    joined_at: BTreeMap<String, DateTime<Utc>>,
    last_event_at: Option<DateTime<Utc>>,
}

impl PlayerActivityTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_snapshot(snapshot: PlayerActivitySnapshot) -> Option<Self> {
        if !matches!(
            snapshot.schema_version,
            LEGACY_PLAYER_ACTIVITY_SCHEMA_VERSION | PLAYER_ACTIVITY_SCHEMA_VERSION
        ) {
            return None;
        }
        let PlayerActivitySnapshot {
            current_players,
            current_player_joined_at,
            players,
            baseline,
            last_event_at,
            ..
        } = snapshot;
        if current_players.len() != current_players.iter().collect::<BTreeSet<_>>().len()
            || current_players.iter().any(|name| !valid_name(name))
            || players.iter().any(|player| !valid_name(&player.name))
        {
            return None;
        }
        let current: BTreeSet<_> = current_players.into_iter().collect();
        let mut joined_at = BTreeMap::new();
        for active in current_player_joined_at {
            if !valid_name(&active.name)
                || !current.contains(&active.name)
                || joined_at.insert(active.name, active.joined_at).is_some()
            {
                return None;
            }
        }
        let mut player_map = BTreeMap::new();
        for player in players {
            if player_map.insert(player.name.clone(), player).is_some() {
                return None;
            }
        }
        Some(Self {
            baseline,
            current,
            players: player_map,
            joined_at,
            last_event_at,
        })
    }

    /// Returns false for an out-of-order or duplicate transition.
    pub fn apply(&mut self, timestamp: DateTime<Utc>, event: VanillaLogEvent) -> bool {
        if self.last_event_at.is_some_and(|last| timestamp < last) {
            return false;
        }
        let accepted = match event {
            VanillaLogEvent::ServerReady => {
                if self.baseline == PresenceBaseline::Known {
                    false
                } else {
                    self.baseline = PresenceBaseline::Known;
                    true
                }
            }
            VanillaLogEvent::PlayerJoined { name } => self.join(name, timestamp),
            VanillaLogEvent::PlayerLeft { name } => self.leave(&name, timestamp),
            VanillaLogEvent::PlayerDied { name } => {
                if let Some(player) = self.players.get_mut(&name) {
                    player.deaths = player.deaths.saturating_add(1);
                    player.last_seen_at = timestamp;
                    true
                } else {
                    false
                }
            }
        };
        if accepted {
            self.last_event_at = Some(timestamp);
        }
        accepted
    }

    pub fn snapshot(&self) -> PlayerActivitySnapshot {
        PlayerActivitySnapshot {
            schema_version: PLAYER_ACTIVITY_SCHEMA_VERSION,
            baseline: self.baseline,
            current_players: self.current.iter().cloned().collect(),
            current_player_joined_at: self
                .joined_at
                .iter()
                .map(|(name, joined_at)| ActivePlayerJoin {
                    name: name.clone(),
                    joined_at: *joined_at,
                })
                .collect(),
            players: self.players.values().cloned().collect(),
            last_event_at: self.last_event_at,
        }
    }

    /// Close every open interval exactly once, for example when a session is
    /// finalized after the process has stopped without emitting leave lines.
    pub fn finalize(&mut self, timestamp: DateTime<Utc>) {
        let current_players: Vec<_> = self.current.iter().cloned().collect();
        for name in current_players {
            let _ = self.leave(&name, timestamp);
        }
    }

    pub fn summary_at(&self, now: DateTime<Utc>) -> PlayerActivitySummary {
        let mut total = self
            .players
            .values()
            .map(|player| player.total_playtime_seconds)
            .sum::<u64>();
        for name in &self.current {
            if let Some(joined_at) = self.joined_at.get(name) {
                total = total.saturating_add(non_negative_seconds(*joined_at, now));
            }
        }
        PlayerActivitySummary {
            baseline: self.baseline,
            current_players: self.current.iter().cloned().collect(),
            known_players: self.players.keys().cloned().collect(),
            total_playtime_seconds: total,
        }
    }

    fn join(&mut self, name: String, timestamp: DateTime<Utc>) -> bool {
        if !valid_name(&name) || !self.current.insert(name.clone()) {
            return false;
        }
        let player = self.players.entry(name.clone()).or_insert(PlayerActivity {
            name: name.clone(),
            first_seen_at: timestamp,
            last_seen_at: timestamp,
            total_playtime_seconds: 0,
            deaths: 0,
        });
        player.last_seen_at = timestamp;
        self.joined_at.insert(name, timestamp);
        true
    }

    fn leave(&mut self, name: &str, timestamp: DateTime<Utc>) -> bool {
        if !self.current.remove(name) {
            return false;
        }
        if let Some(player) = self.players.get_mut(name) {
            player.last_seen_at = timestamp;
            if let Some(joined_at) = self.joined_at.remove(name) {
                player.total_playtime_seconds = player
                    .total_playtime_seconds
                    .saturating_add(non_negative_seconds(joined_at, timestamp));
            }
        }
        true
    }
}

pub fn parse_vanilla_log_line(line: &str) -> Option<VanillaLogEvent> {
    let message = line
        .split_once("]: ")
        .map_or(line, |(_, message)| message)
        .trim();
    if message.starts_with("Done (") && message.contains(")! For help") {
        return Some(VanillaLogEvent::ServerReady);
    }
    for suffix in [" joined the game", " left the game"] {
        if let Some(name) = message.strip_suffix(suffix) {
            let name = name.trim();
            if valid_name(name) {
                return Some(if suffix.starts_with(" joined") {
                    VanillaLogEvent::PlayerJoined { name: name.into() }
                } else {
                    VanillaLogEvent::PlayerLeft { name: name.into() }
                });
            }
        }
    }
    if let Some((name, _reason)) = message.split_once(" lost connection") {
        let name = name.trim_end_matches(':').trim();
        if valid_name(name) {
            return Some(VanillaLogEvent::PlayerLeft { name: name.into() });
        }
    }
    for marker in [
        " was slain by ",
        " was shot by ",
        " was blown up by ",
        " was killed by ",
        " fell from a high place",
        " hit the ground too hard",
        " died",
    ] {
        if let Some(name) = message
            .strip_suffix(marker)
            .or_else(|| message.split_once(marker).map(|(name, _)| name))
        {
            let name = name.trim();
            if valid_name(name) {
                return Some(VanillaLogEvent::PlayerDied { name: name.into() });
            }
        }
    }
    None
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_PLAYER_NAME_BYTES
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
}

fn non_negative_seconds(start: DateTime<Utc>, end: DateTime<Utc>) -> u64 {
    end.signed_duration_since(start)
        .max(Duration::zero())
        .num_seconds()
        .try_into()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn time(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(seconds, 0).expect("timestamp")
    }

    #[test]
    fn parser_recognizes_ready_join_leave_and_death() {
        assert_eq!(
            parse_vanilla_log_line("[Server thread/INFO]: Done (1.2s)! For help, type \"help\""),
            Some(VanillaLogEvent::ServerReady)
        );
        assert_eq!(
            parse_vanilla_log_line("[Server thread/INFO]: Alex joined the game"),
            Some(VanillaLogEvent::PlayerJoined {
                name: "Alex".into()
            })
        );
        assert_eq!(
            parse_vanilla_log_line("[Server thread/INFO]: Alex left the game"),
            Some(VanillaLogEvent::PlayerLeft {
                name: "Alex".into()
            })
        );
        assert_eq!(
            parse_vanilla_log_line("[Server thread/INFO]: Alex lost connection: Disconnected"),
            Some(VanillaLogEvent::PlayerLeft {
                name: "Alex".into()
            })
        );
        assert_eq!(
            parse_vanilla_log_line("[Server thread/INFO]: Alex was slain by Zombie"),
            Some(VanillaLogEvent::PlayerDied {
                name: "Alex".into()
            })
        );
    }

    #[test]
    fn baseline_is_unknown_until_ready_and_transitions_are_deduplicated() {
        let mut tracker = PlayerActivityTracker::new();
        assert_eq!(
            tracker.summary_at(time(10)).baseline,
            PresenceBaseline::Unknown
        );
        assert!(tracker.apply(time(1), VanillaLogEvent::ServerReady));
        assert!(tracker.apply(
            time(2),
            VanillaLogEvent::PlayerJoined {
                name: "Alex".into()
            }
        ));
        assert!(!tracker.apply(
            time(3),
            VanillaLogEvent::PlayerJoined {
                name: "Alex".into()
            }
        ));
        assert!(tracker.apply(
            time(5),
            VanillaLogEvent::PlayerLeft {
                name: "Alex".into()
            }
        ));
        assert_eq!(tracker.summary_at(time(10)).total_playtime_seconds, 3);
    }

    #[test]
    fn out_of_order_events_do_not_reopen_a_closed_interval() {
        let mut tracker = PlayerActivityTracker::new();
        assert!(tracker.apply(time(1), VanillaLogEvent::ServerReady));
        assert!(tracker.apply(
            time(2),
            VanillaLogEvent::PlayerJoined {
                name: "Alex".into()
            }
        ));
        assert!(tracker.apply(
            time(4),
            VanillaLogEvent::PlayerLeft {
                name: "Alex".into()
            }
        ));
        assert!(!tracker.apply(
            time(3),
            VanillaLogEvent::PlayerJoined {
                name: "Alex".into()
            }
        ));
        assert!(tracker.snapshot().current_players.is_empty());
    }

    #[test]
    fn active_join_timestamps_survive_snapshot_restore() {
        let mut tracker = PlayerActivityTracker::new();
        assert!(tracker.apply(time(1), VanillaLogEvent::ServerReady));
        assert!(tracker.apply(
            time(2),
            VanillaLogEvent::PlayerJoined {
                name: "Alex".into()
            }
        ));

        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.current_player_joined_at[0].joined_at, time(2));
        let mut restored = PlayerActivityTracker::from_snapshot(snapshot).expect("restore");
        assert_eq!(restored.summary_at(time(5)).total_playtime_seconds, 3);
        assert!(restored.apply(
            time(7),
            VanillaLogEvent::PlayerLeft {
                name: "Alex".into()
            }
        ));
        assert_eq!(restored.summary_at(time(8)).total_playtime_seconds, 5);
    }

    #[test]
    fn finalize_closes_open_intervals_only_once() {
        let mut tracker = PlayerActivityTracker::new();
        assert!(tracker.apply(time(1), VanillaLogEvent::ServerReady));
        assert!(tracker.apply(
            time(2),
            VanillaLogEvent::PlayerJoined {
                name: "Alex".into()
            }
        ));
        tracker.finalize(time(5));
        assert!(tracker.snapshot().current_players.is_empty());
        assert_eq!(tracker.summary_at(time(10)).total_playtime_seconds, 3);
        tracker.finalize(time(20));
        assert_eq!(tracker.summary_at(time(20)).total_playtime_seconds, 3);
    }
}
