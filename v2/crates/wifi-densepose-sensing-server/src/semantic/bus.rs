//! Semantic event bus — dispatches one [`RawSnapshot`] to every
//! primitive in the order they were registered, collects the
//! [`SemanticEvent`]s emitted, and hands them to MQTT + Matter
//! publishers via a shared `tokio::broadcast` (wiring lives in the
//! publisher, see `mqtt::publisher`).
//!
//! Per §3.12.6 — adding a new primitive is one file change. The bus
//! holds a list of trait objects so the call site doesn't grow when we
//! add primitives in P4.5b.

use super::common::{PrimitiveConfig, PrimitiveState, RawSnapshot, Reason};
use super::{bathroom::BathroomOccupied, no_movement::NoMovement, room_active::RoomActive, sleeping::SomeoneSleeping};

/// Identifier for which primitive produced an event. Used by the
/// publisher to map onto the matching `EntityKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SemanticKind {
    SomeoneSleeping,
    RoomActive,
    BathroomOccupied,
    NoMovement,
    // P4.5b: Distress, ElderlyAnomaly, Meeting, FallRisk, BedExit, MultiRoom.
}

/// One event published to MQTT / Matter consumers.
#[derive(Debug, Clone, PartialEq)]
pub struct SemanticEvent {
    pub kind: SemanticKind,
    pub state: PrimitiveState,
    pub node_id: String,
    pub timestamp_ms: i64,
}

/// Collection of every primitive FSM. Owned by the publisher task.
pub struct SemanticBus {
    sleeping: SomeoneSleeping,
    room_active: RoomActive,
    bathroom: BathroomOccupied,
    no_movement: NoMovement,
    pub config: PrimitiveConfig,
}

impl SemanticBus {
    pub fn new(config: PrimitiveConfig) -> Self {
        Self {
            sleeping: SomeoneSleeping::new(),
            room_active: RoomActive::new(),
            bathroom: BathroomOccupied::new(),
            no_movement: NoMovement::new(),
            config,
        }
    }

    /// Run all primitives on one snapshot. Returns only events that
    /// emit (Idle states are filtered).
    pub fn tick(&mut self, snap: &RawSnapshot) -> Vec<SemanticEvent> {
        let pairs: [(SemanticKind, PrimitiveState); 4] = [
            (SemanticKind::SomeoneSleeping, self.sleeping.tick(snap, &self.config)),
            (SemanticKind::RoomActive,      self.room_active.tick(snap, &self.config)),
            (SemanticKind::BathroomOccupied, self.bathroom.tick(snap, &self.config)),
            (SemanticKind::NoMovement,      self.no_movement.tick(snap, &self.config)),
        ];
        pairs
            .into_iter()
            .filter_map(|(kind, state)| match state {
                PrimitiveState::Idle => None,
                _ => Some(SemanticEvent {
                    kind,
                    state,
                    node_id: snap.node_id.clone(),
                    timestamp_ms: snap.timestamp_ms,
                }),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn cfg() -> PrimitiveConfig {
        PrimitiveConfig::default()
    }

    #[test]
    fn bus_returns_empty_during_warmup() {
        let mut bus = SemanticBus::new(cfg());
        let snap = RawSnapshot {
            since_start: Duration::from_secs(30),
            presence: true,
            motion: 0.5,
            ..Default::default()
        };
        assert!(bus.tick(&snap).is_empty());
    }

    #[test]
    fn bus_emits_room_active_on_sustained_motion() {
        let mut bus = SemanticBus::new(cfg());
        let snap = RawSnapshot {
            node_id: "test".into(),
            since_start: Duration::from_secs(120),
            timestamp_ms: 1_000,
            presence: true,
            motion: 0.4,
            ..Default::default()
        };
        let events = bus.tick(&snap);
        assert!(events.iter().any(|e| e.kind == SemanticKind::RoomActive));
    }

    #[test]
    fn bus_emits_bathroom_when_zone_active() {
        let mut bus = SemanticBus::new(cfg());
        let snap = RawSnapshot {
            node_id: "test".into(),
            since_start: Duration::from_secs(120),
            timestamp_ms: 1_000,
            presence: true,
            active_zones: vec!["bathroom".into()],
            ..Default::default()
        };
        let events = bus.tick(&snap);
        assert!(events.iter().any(|e| e.kind == SemanticKind::BathroomOccupied));
    }

    #[test]
    fn bus_supports_multiple_simultaneous_primitives() {
        let mut bus = SemanticBus::new(cfg());
        let snap = RawSnapshot {
            node_id: "test".into(),
            since_start: Duration::from_secs(120),
            timestamp_ms: 1_000,
            presence: true,
            motion: 0.4,
            active_zones: vec!["bathroom".into()],
            ..Default::default()
        };
        let events = bus.tick(&snap);
        // Both RoomActive AND BathroomOccupied should fire.
        let kinds: Vec<_> = events.iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&SemanticKind::RoomActive));
        assert!(kinds.contains(&SemanticKind::BathroomOccupied));
    }

    #[test]
    fn semantic_event_carries_node_id_and_ts() {
        let mut bus = SemanticBus::new(cfg());
        let snap = RawSnapshot {
            node_id: "aabb".into(),
            since_start: Duration::from_secs(120),
            timestamp_ms: 1779_512_400_000,
            presence: true,
            active_zones: vec!["bathroom".into()],
            ..Default::default()
        };
        let events = bus.tick(&snap);
        let bath = events.into_iter().find(|e| e.kind == SemanticKind::BathroomOccupied).unwrap();
        assert_eq!(bath.node_id, "aabb");
        assert_eq!(bath.timestamp_ms, 1779_512_400_000);
    }

    #[test]
    fn semantic_event_includes_explanation_reason() {
        // Verify that primitives populate the explanation field —
        // critical for HA users debugging automations.
        let mut bus = SemanticBus::new(cfg());
        let snap = RawSnapshot {
            node_id: "test".into(),
            since_start: Duration::from_secs(120),
            timestamp_ms: 1_000,
            presence: true,
            motion: 0.4,
            ..Default::default()
        };
        let events = bus.tick(&snap);
        let ra = events.into_iter().find(|e| e.kind == SemanticKind::RoomActive).unwrap();
        if let PrimitiveState::Boolean { reason, .. } = ra.state {
            assert!(!reason.tags.is_empty(), "reason tags must explain why primitive fired");
        } else {
            panic!("expected Boolean state");
        }
    }

    #[test]
    fn _unused_reason_helper_remains_constructible() {
        // Touch Reason::empty to keep clippy happy when the bus uses
        // it indirectly via primitives.
        let _ = Reason::empty();
    }
}
