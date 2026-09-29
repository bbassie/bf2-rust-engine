//! What a bot knows about enemies it can't see right now: a short memory of contacts (seen,
//! heard, spotted by the team, or shooting at it) whose positions grow uncertain and are
//! forgotten after a while, and suppression from bullets passing close.

use bevy::prelude::*;

/// How a bot learned about an enemy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    /// Heard firing or walking nearby: a rough position.
    Heard,
    /// Spotted by the team (commo rose, UAV, a teammate's sighting).
    Team,
    /// Shots passing close or hitting it came from there.
    Shot,
    /// Seen with its own eyes.
    Seen,
}

/// A remembered enemy.
#[derive(Clone, Copy, Debug)]
pub struct Contact {
    pub enemy: Entity,
    /// Where he was (chest height), as well as it knows.
    pub position: Vec3,
    /// Seconds since that was learned.
    pub age: f32,
    pub source: Source,
}

/// A bot's memory of enemies, most recent first once sorted.
#[derive(Default, Debug)]
pub struct Memory {
    pub contacts: Vec<Contact>,
}

/// Contacts kept at most.
const MAX_CONTACTS: usize = 8;

impl Memory {
    /// Learns (or refreshes) where an enemy is. A worse source doesn't overwrite a fresh
    /// better one.
    pub fn learn(&mut self, enemy: Entity, position: Vec3, source: Source) {
        if let Some(c) = self.contacts.iter_mut().find(|c| c.enemy == enemy) {
            if source >= c.source || c.age > 1.5 {
                c.position = position;
                c.age = 0.0;
                c.source = source;
            }
            return;
        }
        if self.contacts.len() >= MAX_CONTACTS {
            // Forget the oldest.
            if let Some(oldest) = self
                .contacts
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.age.total_cmp(&b.1.age))
                .map(|(i, _)| i)
            {
                self.contacts.swap_remove(oldest);
            }
        }
        self.contacts.push(Contact {
            enemy,
            position,
            age: 0.0,
            source,
        });
    }

    /// Ages contacts and forgets those older than `memory` seconds or no longer there.
    pub fn tick(&mut self, dt: f32, memory: f32, exists: impl Fn(Entity) -> bool) {
        for c in &mut self.contacts {
            c.age += dt;
        }
        self.contacts.retain(|c| c.age < memory && exists(c.enemy));
    }

    pub fn forget(&mut self, enemy: Entity) {
        self.contacts.retain(|c| c.enemy != enemy);
    }

    pub fn clear(&mut self) {
        self.contacts.clear();
    }

    /// The contact that matters most from `position`: recent and close, sightings first.
    pub fn most_pressing(&self, position: Vec3, max_age: f32) -> Option<&Contact> {
        self.contacts
            .iter()
            .filter(|c| c.age < max_age)
            .min_by(|a, b| pressing(a, position).total_cmp(&pressing(b, position)))
    }

    /// Contacts within `radius` of `position` learned in the last `max_age` seconds.
    pub fn near(&self, position: Vec3, radius: f32, max_age: f32) -> impl Iterator<Item = &Contact> + '_ {
        self.contacts
            .iter()
            .filter(move |c| c.age < max_age && c.position.distance_squared(position) < radius * radius)
    }

    /// The average direction (yaw-free, XZ unit vector) of recent contacts from `position`,
    /// weighted towards close and fresh ones: the threat axis.
    pub fn threat_axis(&self, position: Vec3, max_age: f32) -> Option<Vec3> {
        let mut sum = Vec3::ZERO;
        for c in self.contacts.iter().filter(|c| c.age < max_age) {
            let to = (c.position - position).with_y(0.0);
            let d = to.length();
            if d > 1.0 {
                sum += to / d * (1.0 / (1.0 + c.age)) * (60.0 / d.max(20.0));
            }
        }
        (sum.length() > 1e-3).then(|| sum.normalize())
    }
}

fn pressing(c: &Contact, position: Vec3) -> f32 {
    let source = match c.source {
        Source::Seen => 0.0,
        Source::Shot => 5.0,
        Source::Team => 15.0,
        Source::Heard => 20.0,
    };
    c.position.distance(position) + 6.0 * c.age + source
}

/// Suppression from a burst passing `miss` meters from a soldier (0 at [`NEAR_MISS`]),
/// per second of fire.
pub fn suppression_rate(miss: f32) -> f32 {
    2.4 * (1.0 - miss / NEAR_MISS).clamp(0.0, 1.0) + 0.4
}

/// Bullets passing closer than this suppress (the client's flyby crack radius).
pub const NEAR_MISS: f32 = 4.0;

#[cfg(test)]
mod tests {
    use super::*;

    fn entity(n: u32) -> Entity {
        Entity::from_raw_u32(n).unwrap()
    }

    #[test]
    fn remembers_and_forgets() {
        let mut memory = Memory::default();
        memory.learn(entity(1), Vec3::new(10.0, 0.0, 0.0), Source::Seen);
        memory.learn(entity(2), Vec3::new(50.0, 0.0, 0.0), Source::Heard);
        // A rough position doesn't overwrite a fresh sighting.
        memory.learn(entity(1), Vec3::new(30.0, 0.0, 0.0), Source::Heard);
        assert_eq!(memory.contacts[0].position.x, 10.0);
        assert_eq!(memory.most_pressing(Vec3::ZERO, 10.0).unwrap().enemy, entity(1));
        memory.tick(5.0, 12.0, |_| true);
        memory.learn(entity(2), Vec3::new(40.0, 0.0, 0.0), Source::Heard);
        memory.tick(8.0, 12.0, |_| true);
        assert_eq!(memory.contacts.len(), 1, "the old sighting is forgotten");
        assert_eq!(memory.contacts[0].enemy, entity(2));
        let axis = memory.threat_axis(Vec3::ZERO, 20.0).unwrap();
        assert!(axis.x > 0.99);
    }
}
