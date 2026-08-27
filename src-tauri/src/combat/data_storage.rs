use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::RwLock;

use crate::entity::damage_packet::ParsedDamagePacket;
use crate::entity::job_class::JobClass;
use crate::entity::special_damage::SpecialDamage;
use crate::entity::summon_resolver;

/// Maximum idle gap before a fight is considered ended and a new one begins.
const IDLE_RESET_MS: i64 = 30_000;

/// A zone-change auto-reset is ignored if any damage was recorded within this
/// window, so an in-combat self-teleport (boss knockback/pull) can't wipe an
/// active fight. Real zone transitions always follow a travel/load lull.
const ZONE_RESET_LULL_MS: i64 = 1_500;
/// Minimum spacing between two zone-change resets (debounce).
const ZONE_RESET_DEBOUNCE_MS: i64 = 4_000;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ───── Aggregate data structures ─────

/// Healing done, aggregated per (healer actor, skill, is_hot). Healing is keyed by
/// the HEALER (not the boss target), since the meter shows "healing done" per player.
#[derive(Debug, Clone, Default)]
pub struct HealSkillData {
    pub total_heal: i64,
    pub tick_count: i32,
}

#[derive(Debug, Clone)]
pub struct SkillCombatData {
    pub skill_code: i32,
    pub is_dot: bool,
    pub hit_count: i32,
    pub total_damage: i32,
    pub min_damage: i32,
    pub max_damage: i32,
    pub crit_count: i32,
    pub back_count: i32,
    pub frontal_count: i32,
    pub parry_count: i32,
    pub perfect_count: i32,
    pub double_count: i32,
    pub smite_count: i32,
    pub powershard_count: i32,
    pub multi_hit_count: i32,
    pub multi_hit_damage: i32,
    pub multi_hit_hits: i32,
    pub heal_amount: i32,
    pub hit_timestamps: Vec<i64>,
    pub spec_flags: [bool; 5],
}

impl SkillCombatData {
    /// Clone every aggregate field but leave `hit_timestamps` empty.
    /// The timestamp Vec grows by one entry per hit (unbounded over a long
    /// fight) and is only ever consumed by `get_target_details` (the details
    /// panel chart). Every other consumer clones it for nothing, so the hot
    /// 500ms paths use this to keep per-tick clone cost flat over fight time.
    /// Spelled out manually rather than `Vec::new(), ..self.clone()` because
    /// the latter would copy `hit_timestamps` only to throw it away.
    fn clone_light(&self) -> Self {
        Self {
            skill_code: self.skill_code,
            is_dot: self.is_dot,
            hit_count: self.hit_count,
            total_damage: self.total_damage,
            min_damage: self.min_damage,
            max_damage: self.max_damage,
            crit_count: self.crit_count,
            back_count: self.back_count,
            frontal_count: self.frontal_count,
            parry_count: self.parry_count,
            perfect_count: self.perfect_count,
            double_count: self.double_count,
            smite_count: self.smite_count,
            powershard_count: self.powershard_count,
            multi_hit_count: self.multi_hit_count,
            multi_hit_damage: self.multi_hit_damage,
            multi_hit_hits: self.multi_hit_hits,
            heal_amount: self.heal_amount,
            hit_timestamps: Vec::new(),
            spec_flags: self.spec_flags,
        }
    }

    fn new(skill_code: i32, is_dot: bool) -> Self {
        Self {
            skill_code,
            is_dot,
            hit_count: 0,
            total_damage: 0,
            min_damage: i32::MAX,
            max_damage: 0,
            crit_count: 0,
            back_count: 0,
            frontal_count: 0,
            parry_count: 0,
            perfect_count: 0,
            double_count: 0,
            smite_count: 0,
            powershard_count: 0,
            multi_hit_count: 0,
            multi_hit_damage: 0,
            multi_hit_hits: 0,
            heal_amount: 0,
            hit_timestamps: Vec::new(),
            spec_flags: [false; 5],
        }
    }
}

/// One entry of the party roster packet (`0x9702`). Keyed by character name,
/// because the roster carries the account-level `dbid` rather than the
/// session-scoped entity id — the name is the only field that joins it to the
/// in-world entities the meter tracks.
#[derive(Debug, Clone, Default)]
pub struct PartyMember {
    /// 1-based party slot.
    pub slot: u8,
    pub level: i32,
    /// Equipment item level ("gear score").
    pub gear_score: i32,
    /// Combat power — the number the game shows on the character sheet.
    pub combat_power: i64,
    /// World/server id (the bracket tag next to a cross-server player's name).
    pub server_id: u16,
}

#[derive(Debug, Clone)]
pub struct ActorCombatData {
    pub total_damage: i64,
    pub party_heal: i64,
    pub regen: i64,
    pub damage_received: i64,
    pub hits_received: i32,
    pub last_damage_time: i64,
    pub job: Option<JobClass>,
    /// Skills keyed by (raw_skill_code, is_dot)
    pub skills: HashMap<(i32, bool), SkillCombatData>,
}

impl ActorCombatData {
    fn new() -> Self {
        Self {
            total_damage: 0,
            party_heal: 0,
            regen: 0,
            damage_received: 0,
            hits_received: 0,
            last_damage_time: 0,
            job: None,
            skills: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TargetCombatData {
    pub target_id: i32,
    pub total_damage: i64,
    pub first_damage_time: i64,
    pub last_damage_time: i64,
    pub last_packet_id: i64,
    /// Per raw-actor aggregated combat data
    pub actors: HashMap<i32, ActorCombatData>,
}

impl TargetCombatData {
    fn new(target_id: i32, timestamp: i64) -> Self {
        Self {
            target_id,
            total_damage: 0,
            first_damage_time: timestamp,
            last_damage_time: timestamp,
            last_packet_id: -1,
            actors: HashMap::new(),
        }
    }
}

// ───── Main storage ─────

pub struct DataStorage {
    inner: RwLock<Inner>,
    damage_generation: AtomicI64,
    /// Wall-clock ms of the last damage record — gates the zone-change lull check.
    last_damage_ms: AtomicI64,
    /// Wall-clock ms of the last honored zone-change reset — debounce.
    last_zone_reset_ms: AtomicI64,
    /// Set when a zone change clears combat; the dps calculator consumes it to
    /// drop its cached snapshot / saved-target state on the next cycle.
    combat_reset_requested: AtomicBool,
}

struct Inner {
    /// Aggregated combat data per target (replaces raw packet storage)
    target_combat: HashMap<i32, TargetCombatData>,
    /// Job class detected per actor (across all targets, for summon matching)
    actor_jobs: HashMap<i32, JobClass>,

    nickname_storage: HashMap<i32, String>,
    pending_nicknames: HashMap<i32, String>,
    permanent_nicknames: HashMap<i32, String>,
    summon_storage: HashMap<i32, i32>,
    mob_storage: HashMap<i32, i32>,
    /// Healing done per (healer actor) -> (skill_code, is_hot) -> aggregate.
    heal_storage: HashMap<i32, HashMap<(i32, bool), HealSkillData>>,
    /// Spawn-time / observed-peak MAX HP per entity (denominator for the HP bar).
    mob_hp_data: HashMap<i32, i32>,
    /// Live CURRENT HP per entity, from the in-place `8D <id> 02 01 00 <u32>` feed.
    mob_current_hp: HashMap<i32, i32>,
    known_player_ids: HashSet<i32>,
    /// Ids whose nickname came from an authoritative source (a 45/44 36 player
    /// spawn or the account char-list). Lower-confidence parsers may not steal
    /// such a name onto a different id. Lives parallel to `nickname_storage`:
    /// survives a combat flush, cleared by `reset_nicknames`, evicted alongside.
    authoritative_name_ids: HashSet<i32>,
    confirmed_summon_ids: HashSet<i32>,
    /// Ids that spawned via a `40/41 36` mob/summon spawn (as opposed to a
    /// `44/45 36` player spawn). A real player never spawns this way, so an
    /// entity here that deals class-band damage is a summon / spell-effect
    /// entity — it must not be flagged as a known player, and is a candidate for
    /// attribution to the same-class player.
    summon_spawn_ids: HashSet<i32>,
    /// Entity ids below the usual `>= 100` sanity floor that a spawn or identity
    /// record has proven real. The damage parser uses `>= 100` as a resync gate
    /// while walking varints, which silently discarded every hit from players
    /// whose session entity id happened to be tiny (observed live: an
    /// Elementalist at id 48 lost 939 hits plus all 114 of their pets). Ids
    /// confirmed here are allowed through that gate; unconfirmed low values are
    /// still rejected, so the gate keeps its resync value.
    low_id_entities: HashSet<i32>,
    /// Party roster from the `0x9702` packet, keyed by character name.
    party_members: HashMap<String, PartyMember>,
    /// Power-scalar values observed per actor in its damage records. A summon
    /// inherits its owner's, so this links the two when no spawn packet (and
    /// therefore no `parent_key`) ever arrives — the case for a Cleric's Divine
    /// Aura, which the server creates without announcing. A set rather than one
    /// value because the scalar shifts as buffs come and go, and owner and summon
    /// are not always in the same buff state at the same instant.
    actor_power_scalars: HashMap<i32, HashSet<i32>>,
    hostile_target_ids: HashSet<i32>,
    dead_entity_ids: HashSet<i32>,
    /// Boss entity IDs identified from NPC DB boss flags
    boss_entity_ids: HashSet<i32>,
    /// Whether the current combat segment has any boss damage
    has_boss_in_segment: bool,
    current_target: i32,

    // Local player
    local_player_id: Option<i64>,
    local_character_name: Option<String>,
}

impl DataStorage {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(Inner {
                target_combat: HashMap::new(),
                actor_jobs: HashMap::new(),
                nickname_storage: HashMap::new(),
                pending_nicknames: HashMap::new(),
                permanent_nicknames: HashMap::new(),
                summon_storage: HashMap::new(),
                mob_storage: HashMap::new(),
                heal_storage: HashMap::new(),
                mob_hp_data: HashMap::new(),
                mob_current_hp: HashMap::new(),
                known_player_ids: HashSet::new(),
                authoritative_name_ids: HashSet::new(),
                confirmed_summon_ids: HashSet::new(),
                summon_spawn_ids: HashSet::new(),
                low_id_entities: HashSet::new(),
                party_members: HashMap::new(),
                actor_power_scalars: HashMap::new(),
                hostile_target_ids: HashSet::new(),
                dead_entity_ids: HashSet::new(),
                boss_entity_ids: HashSet::new(),
                has_boss_in_segment: false,
                current_target: 0,
                local_player_id: None,
                local_character_name: None,
            }),
            damage_generation: AtomicI64::new(0),
            last_damage_ms: AtomicI64::new(0),
            last_zone_reset_ms: AtomicI64::new(0),
            combat_reset_requested: AtomicBool::new(false),
        }
    }

    /// Called when a self/world teleport (zone-change opcode) is seen. Resets
    /// combat data only if not in active combat (lull) and not recently reset
    /// (debounce), so the meter starts clean on entering a dungeon/instance
    /// without ever wiping an in-progress fight. Returns true if it reset.
    pub fn note_zone_change(&self) -> bool {
        let now = now_ms();
        if now - self.last_damage_ms.load(Ordering::Relaxed) < ZONE_RESET_LULL_MS {
            return false; // mid-combat teleport — ignore
        }
        if now - self.last_zone_reset_ms.load(Ordering::Relaxed) < ZONE_RESET_DEBOUNCE_MS {
            return false; // already reset moments ago
        }
        {
            let inner = self.inner.read();
            if inner.target_combat.is_empty() {
                return false; // nothing to clear
            }
        }
        self.last_zone_reset_ms.store(now, Ordering::Relaxed);
        // Preserve identity across the reset: a teleport within the same instance
        // keeps everyone's entity ids, so wiping nicknames/known-players/summons
        // would drop your party (and you) to raw ids until they happen to be
        // re-broadcast. Clear only the per-segment damage aggregates.
        self.flush_combat_only();
        self.combat_reset_requested.store(true, Ordering::Relaxed);
        tracing::info!("Zone change detected — combat data reset (identity preserved)");
        true
    }

    /// Consumed by the dps calculator to drop its cached snapshot/saved-target
    /// state after a zone-change combat reset.
    pub fn take_combat_reset_requested(&self) -> bool {
        self.combat_reset_requested.swap(false, Ordering::Relaxed)
    }

    pub fn damage_generation(&self) -> i64 {
        self.damage_generation.load(Ordering::Relaxed)
    }

    pub fn set_local_character_name(&self, name: Option<String>) {
        self.inner.write().local_character_name = name;
    }

    pub fn local_character_name(&self) -> Option<String> {
        self.inner.read().local_character_name.clone()
    }

    pub fn set_local_player_id(&self, id: Option<i64>) {
        self.inner.write().local_player_id = id;
    }

    pub fn local_player_id(&self) -> Option<i64> {
        self.inner.read().local_player_id
    }

    pub fn append_damage(&self, pdp: ParsedDamagePacket) {
        let mut inner = self.inner.write();
        let skill_code = pdp.skill_code();
        let actor_id = pdp.actor_id();
        let target_id = pdp.target_id();

        // NPC actors using NPC skills: track damage received on the player target, then skip
        let uses_npc_skill = (1_000_000..=9_999_999).contains(&skill_code);
        if inner.mob_storage.contains_key(&actor_id)
            && !inner.summon_storage.contains_key(&actor_id)
            && uses_npc_skill
        {
            // Track damage received on the player target
            let resolved_target = summon_resolver::resolve(target_id, &inner.summon_storage);
            if inner.known_player_ids.contains(&resolved_target) {
                let dmg = pdp.total_damage() as i64;
                for target_data in inner.target_combat.values_mut() {
                    if let Some(actor_data) = target_data.actors.get_mut(&resolved_target) {
                        actor_data.damage_received += dmg;
                        actor_data.hits_received += 1;
                        break;
                    }
                }
            }
            return;
        }

        // Track player skill usage. Exclude anything that spawned via a mob/summon
        // spawn (40/41 36): a real player never does, so such an entity dealing
        // class-band damage is a summon / spell-effect, not a player.
        if is_player_skill(skill_code)
            && !inner.confirmed_summon_ids.contains(&actor_id)
            && !inner.summon_spawn_ids.contains(&actor_id)
        {
            let is_new = inner.known_player_ids.insert(actor_id);
            if is_new {
                inner.summon_storage.remove(&actor_id);
                purge_friendly_damage(&mut inner, actor_id);
            }
        }

        // Party healing: player-on-player damage is actually healing/buffs
        if is_friendly_action(&inner, actor_id, target_id) {
            let heal_amount = pdp.total_damage();
            if heal_amount > 0 {
                // Record party heal on the actor's data in all targets they appear in
                for target_data in inner.target_combat.values_mut() {
                    if let Some(actor_data) = target_data.actors.get_mut(&actor_id) {
                        actor_data.party_heal += heal_amount as i64;
                        break;
                    }
                }
                // Also record per-skill so ally heals show in the HEAL view (the
                // self-heal path does this via append_heal; mirror it for ally heals).
                let e = inner
                    .heal_storage
                    .entry(actor_id)
                    .or_default()
                    .entry((pdp.skill_code(), false))
                    .or_default();
                e.total_heal += heal_amount as i64;
                e.tick_count += 1;
            }
            return;
        }

        // Track hostile targets
        let resolved = summon_resolver::resolve(actor_id, &inner.summon_storage);
        if inner.known_player_ids.contains(&resolved) {
            inner.hostile_target_ids.insert(target_id);
        }

        // Track actor job
        if let Some(job) = JobClass::convert_from_skill(skill_code) {
            inner.actor_jobs.entry(actor_id).or_insert(job);
        }

        // Boss encounter auto-reset: if this target is a boss and the current
        // segment has no boss yet, clear the trash segment so boss gets clean data.
        let is_boss_target = inner.boss_entity_ids.contains(&target_id);
        if is_boss_target && !inner.has_boss_in_segment && !inner.target_combat.is_empty() {
            tracing::info!("Boss encounter auto-reset: boss entity {} hit, clearing trash segment", target_id);
            inner.target_combat.clear();
            inner.dead_entity_ids.clear();
            inner.has_boss_in_segment = true;
        } else if is_boss_target {
            inner.has_boss_in_segment = true;
        }

        let timestamp = pdp.timestamp();
        let packet_id = pdp.id();

        // Get or create target combat data
        let target_data = inner.target_combat.entry(target_id).or_insert_with(|| {
            TargetCombatData::new(target_id, timestamp)
        });

        // Idle reset check (30s gap)
        if target_data.last_damage_time > 0
            && timestamp - target_data.last_damage_time > IDLE_RESET_MS
        {
            tracing::info!("Idle reset: target {} — gap {}ms", target_id,
                timestamp - target_data.last_damage_time);
            *target_data = TargetCombatData::new(target_id, timestamp);
        }

        // Update target timing
        if timestamp < target_data.first_damage_time {
            target_data.first_damage_time = timestamp;
        }
        if timestamp > target_data.last_damage_time {
            target_data.last_damage_time = timestamp;
        }
        let total_dmg = pdp.total_damage();
        target_data.total_damage += total_dmg as i64;
        target_data.last_packet_id = packet_id;

        // Update actor data within target
        let actor_data = target_data.actors.entry(actor_id).or_insert_with(ActorCombatData::new);
        actor_data.total_damage += total_dmg as i64;
        if timestamp > actor_data.last_damage_time {
            actor_data.last_damage_time = timestamp;
        }
        if actor_data.job.is_none() {
            actor_data.job = JobClass::convert_from_skill(skill_code);
        }

        // Update skill data
        let skill_key = (skill_code, pdp.is_dot());
        let skill_data = actor_data.skills.entry(skill_key).or_insert_with(|| {
            SkillCombatData::new(skill_code, pdp.is_dot())
        });
        skill_data.hit_count += 1;
        // saturating_add: per-skill totals are i32 and a long boss fight can
        // exceed i32::MAX — overflow panics in debug and wraps to negative in
        // release. Cap instead of crashing/wrapping.
        skill_data.total_damage = skill_data.total_damage.saturating_add(total_dmg);
        let hit_dmg = pdp.damage();
        if hit_dmg < skill_data.min_damage { skill_data.min_damage = hit_dmg; }
        if hit_dmg > skill_data.max_damage { skill_data.max_damage = hit_dmg; }
        if pdp.is_crit() { skill_data.crit_count += 1; }
        if pdp.specials().contains(&SpecialDamage::Back) { skill_data.back_count += 1; }
        if pdp.specials().contains(&SpecialDamage::Frontal) { skill_data.frontal_count += 1; }
        if pdp.specials().contains(&SpecialDamage::Parry) { skill_data.parry_count += 1; }
        if pdp.specials().contains(&SpecialDamage::Perfect) { skill_data.perfect_count += 1; }
        if pdp.specials().contains(&SpecialDamage::Double) { skill_data.double_count += 1; }
        if pdp.specials().contains(&SpecialDamage::Smite) { skill_data.smite_count += 1; }
        if pdp.specials().contains(&SpecialDamage::PowerShard) { skill_data.powershard_count += 1; }
        if pdp.multi_hit_count() > 0 {
            skill_data.multi_hit_count += 1;
            skill_data.multi_hit_damage = skill_data.multi_hit_damage.saturating_add(pdp.multi_hit_damage());
            skill_data.multi_hit_hits += pdp.multi_hit_count();
        }
        skill_data.heal_amount = skill_data.heal_amount.saturating_add(pdp.heal_amount());
        // Track regen (life-steal) on the actor aggregate
        if pdp.heal_amount() > 0 {
            actor_data.regen += pdp.heal_amount() as i64;
        }
        skill_data.hit_timestamps.push(timestamp);
        for (i, &flag) in pdp.spec_flags().iter().enumerate() {
            if flag { skill_data.spec_flags[i] = true; }
        }

        self.damage_generation.fetch_add(1, Ordering::Relaxed);
        self.last_damage_ms.store(now_ms(), Ordering::Relaxed);

        // Apply pending nickname
        apply_pending_nickname(&mut inner, actor_id);
    }

    pub fn append_mob(&self, mid: i32, code: i32) {
        let mut inner = self.inner.write();
        inner.mob_storage.insert(mid, code);

        // NPC unclassification: if this entity was previously classified as a player
        // (damage with player-band skills arrived before the 0x3640 spawn packet),
        // undo the classification and scrub ghost player damage from aggregates.
        if inner.known_player_ids.remove(&mid) {
            tracing::trace!("NPC unclassification: entity {} reclassified as mob (code {})", mid, code);
            // Subtract ghost player damage from target totals
            for target_data in inner.target_combat.values_mut() {
                if let Some(actor_data) = target_data.actors.remove(&mid) {
                    target_data.total_damage -= actor_data.total_damage;
                }
            }
        }
    }

    pub fn append_mob_hp(&self, mid: i32, hp: i32) {
        if hp > 0 {
            self.inner.write().mob_hp_data.insert(mid, hp);
        }
    }

    pub fn mark_entity_dead(&self, entity_id: i32) {
        self.inner.write().dead_entity_ids.insert(entity_id);
    }

    pub fn is_entity_dead(&self, entity_id: i32) -> bool {
        self.inner.read().dead_entity_ids.contains(&entity_id)
    }

    pub fn get_dead_entities(&self) -> HashSet<i32> {
        self.inner.read().dead_entity_ids.clone()
    }

    pub fn register_boss(&self, entity_id: i32) {
        self.inner.write().boss_entity_ids.insert(entity_id);
    }

    pub fn is_boss(&self, entity_id: i32) -> bool {
        self.inner.read().boss_entity_ids.contains(&entity_id)
    }

    pub fn is_mob(&self, id: i32) -> bool {
        self.inner.read().mob_storage.contains_key(&id)
    }

    pub fn is_damage_target(&self, id: i32) -> bool {
        self.inner.read().target_combat.contains_key(&id)
    }

    pub fn is_summon(&self, id: i32) -> bool {
        self.inner.read().summon_storage.contains_key(&id)
    }

    pub fn is_confirmed_summon(&self, id: i32) -> bool {
        self.inner.read().confirmed_summon_ids.contains(&id)
    }

    /// Record that `id` appeared in a `40/41 36` mob/summon spawn (never a player
    /// spawn). Used to keep summon / spell-effect entities out of the known-player
    /// set and make them attributable to their same-class player.
    pub fn note_summon_spawn(&self, id: i32) {
        self.inner.write().summon_spawn_ids.insert(id);
    }

    pub fn get_summon_spawn_ids(&self) -> HashSet<i32> {
        self.inner.read().summon_spawn_ids.clone()
    }

    /// Confirm that a sub-100 entity id is a real entity (seen in a spawn or an
    /// identity record), so the damage parser's `>= 100` resync gate lets it
    /// through. See `Inner::low_id_entities`.
    pub fn note_low_id_entity(&self, id: i32) {
        if (1..100).contains(&id) {
            self.inner.write().low_id_entities.insert(id);
        }
    }

    /// True when `id` passes the entity-id sanity gate used while walking damage
    /// records: anything at or above the usual floor, plus tiny ids the game has
    /// explicitly announced.
    pub fn is_plausible_entity_id(&self, id: i32) -> bool {
        if id >= 100 {
            return true;
        }
        id >= 1 && self.inner.read().low_id_entities.contains(&id)
    }

    /// Take a party roster from a `0x9702` packet.
    ///
    /// `complete` says whether every member the packet declared was decoded. A
    /// complete roster replaces what we had, so a member who left the party
    /// disappears; a partial one only updates the members it did decode, so a
    /// record this parser trips over costs that member a refresh rather than
    /// costing the whole party their combat power.
    pub fn set_party_roster(&self, members: Vec<(String, PartyMember)>, complete: bool) {
        if members.is_empty() {
            return;
        }
        let mut inner = self.inner.write();
        if complete {
            inner.party_members.clear();
        }
        for (name, member) in members {
            inner.party_members.insert(name, member);
        }
    }

    pub fn get_party_members(&self) -> HashMap<String, PartyMember> {
        self.inner.read().party_members.clone()
    }

    /// Record a power-scalar reading for an actor. See `Inner::actor_power_scalars`.
    pub fn note_power_scalar(&self, actor_id: i32, scalar: i32) {
        if actor_id <= 0 || scalar <= 0 {
            return;
        }
        let mut inner = self.inner.write();
        let set = inner.actor_power_scalars.entry(actor_id).or_default();
        // Bounded: buff churn produces a handful of distinct values, not many.
        if set.len() < 16 {
            set.insert(scalar);
        }
    }

    pub fn get_power_scalars(&self) -> HashMap<i32, HashSet<i32>> {
        self.inner.read().actor_power_scalars.clone()
    }

    pub fn register_confirmed_summon_by_id(&self, summon_id: i32, owner_id: i32) {
        tracing::trace!("Summon confirmed (5F 00): {} owned by {}", summon_id, owner_id);
        let mut inner = self.inner.write();
        inner.confirmed_summon_ids.insert(summon_id);
        inner.known_player_ids.remove(&summon_id);
        inner.summon_storage.insert(summon_id, owner_id);
        purge_friendly_damage(&mut inner, summon_id);
    }

    pub fn append_summon(&self, summoner: i32, summon: i32) {
        let mut inner = self.inner.write();

        // Guards from Kotlin
        if inner.nickname_storage.contains_key(&summon) { return; }
        if inner.known_player_ids.contains(&summon) { return; }
        if inner.hostile_target_ids.contains(&summon) { return; }
        if inner.summon_storage.contains_key(&summoner) { return; }
        if inner.mob_storage.contains_key(&summoner) && !inner.summon_storage.contains_key(&summoner) { return; }

        // Job compatibility check
        let summon_job = inner.actor_jobs.get(&summon).copied();
        let owner_job = inner.actor_jobs.get(&summoner).copied();
        if let (Some(sj), Some(oj)) = (summon_job, owner_job) {
            if sj != oj { return; }
        }

        tracing::debug!("Summon linked: {} owned by {}", summon, summoner);
        inner.summon_storage.insert(summon, summoner);
    }

    /// Bind a nickname from a LOWER-CONFIDENCE source (fuzzy actor-name rules,
    /// loot attribution, nickname scan). Gated so it can't corrupt naming:
    ///  1. It may only name an id that is already a real entity — seen in combat,
    ///     a known player, spawn-authoritative, a summon, or already named. This
    ///     rejects names being bound to counter/terminator-derived junk ids (e.g.
    ///     the sequence counter in a `0E 00 36 <counter>` record), which would
    ///     otherwise evict a correct spawn name via the name-eviction rule.
    ///  2. It may not steal a name that an authoritative source already bound to a
    ///     different id.
    pub fn append_nickname(&self, uid: i32, nickname: &str) {
        let mut inner = self.inner.write();
        if !fuzzy_bind_allowed(&inner, uid, nickname) {
            return;
        }
        append_nickname_inner(&mut inner, uid, nickname);
    }

    /// Bind a nickname from an AUTHORITATIVE source (a masked identity record, a
    /// 45/44 36 player spawn, or the account char-list). Not gated — the id↔name
    /// pairing is stated by the protocol — and marks the id so fuzzy parsers
    /// can't later steal the name. A stale/junk prior binding of this name is
    /// evicted, reclaiming the name to the real id.
    ///
    /// Applied with `force`, so the length/script heuristics that protect against
    /// bad fuzzy scan results cannot reject a real name. Those heuristics cost a
    /// live capture its Ranger: a fuzzy parser had bound the LEGION name
    /// "BaroqueWorks" to that player, and the "don't replace a longer name with a
    /// short ASCII one" rule then refused their actual name, "M7".
    pub fn append_nickname_authoritative(&self, uid: i32, nickname: &str) {
        let mut inner = self.inner.write();
        inner.authoritative_name_ids.insert(uid);
        append_nickname_inner_with_force(&mut inner, uid, nickname, true);
    }

    pub fn set_permanent_nickname(&self, uid: i32, nickname: &str) {
        let mut inner = self.inner.write();
        inner.permanent_nicknames.insert(uid, nickname.to_string());
        // User explicitly set this in settings: authoritative, and force-apply to
        // bypass length/CJK heuristics that protect against bad packet scan results.
        inner.authoritative_name_ids.insert(uid);
        append_nickname_inner_with_force(&mut inner, uid, nickname, true);
    }

    pub fn cache_pending_nickname(&self, uid: i32, nickname: &str) {
        let mut inner = self.inner.write();
        if inner.nickname_storage.contains_key(&uid) { return; }
        inner.pending_nicknames.insert(uid, nickname.to_string());
    }

    pub fn has_nickname(&self, uid: i32) -> bool {
        self.inner.read().nickname_storage.contains_key(&uid)
    }

    pub fn get_nickname(&self, uid: i32) -> Option<String> {
        self.inner.read().nickname_storage.get(&uid).cloned()
    }

    /// Reverse lookup: find entity ID by nickname (for summon owner resolution).
    pub fn find_id_by_nickname(&self, name: &str) -> Option<i32> {
        let inner = self.inner.read();
        for (&id, nick) in &inner.nickname_storage {
            if nick == name {
                return Some(id);
            }
        }
        None
    }

    pub fn actor_appears_in_combat(&self, actor_id: i32) -> bool {
        let inner = self.inner.read();
        // Check if actor appears as an attacker in any target
        for target_data in inner.target_combat.values() {
            if target_data.actors.contains_key(&actor_id) {
                return true;
            }
        }
        // Check if actor is a target
        if inner.target_combat.contains_key(&actor_id) {
            return true;
        }
        inner.summon_storage.contains_key(&actor_id)
    }

    pub fn get_nicknames(&self) -> HashMap<i32, String> {
        self.inner.read().nickname_storage.clone()
    }

    pub fn get_summon_data(&self) -> HashMap<i32, i32> {
        self.inner.read().summon_storage.clone()
    }

    pub fn get_known_player_ids(&self) -> HashSet<i32> {
        self.inner.read().known_player_ids.clone()
    }

    pub fn is_known_player(&self, id: i32) -> bool {
        self.inner.read().known_player_ids.contains(&id)
    }

    pub fn get_mob_hp_data(&self) -> HashMap<i32, i32> {
        self.inner.read().mob_hp_data.clone()
    }

    pub fn get_mob_hp(&self, id: i32) -> Option<i32> {
        self.inner.read().mob_hp_data.get(&id).copied()
    }

    /// Record a live current-HP reading for an entity (from the `8D ... 02 01 00`
    /// feed). Also seeds/raises the entity's MAX HP from the observed peak, so a
    /// boss whose spawn packet was missed still gets a usable denominator (current
    /// HP never exceeds max in-game, so taking the max never overstates it).
    pub fn set_mob_current_hp(&self, id: i32, hp: i32) {
        if hp < 0 {
            return;
        }
        let mut inner = self.inner.write();
        inner.mob_current_hp.insert(id, hp);
        let max = inner.mob_hp_data.entry(id).or_insert(0);
        if hp > *max {
            *max = hp;
        }
    }

    pub fn get_mob_current_hp(&self, id: i32) -> Option<i32> {
        self.inner.read().mob_current_hp.get(&id).copied()
    }

    /// Record a heal tick done by `actor_id` with `skill_code` (is_hot marks a HoT).
    /// Keyed by the healer so "healing done" can be shown per player. Self-heals count.
    pub fn append_heal(&self, actor_id: i32, skill_code: i32, amount: i64, is_hot: bool) {
        if amount <= 0 || !self.is_plausible_entity_id(actor_id) {
            return;
        }
        let mut inner = self.inner.write();
        let e = inner
            .heal_storage
            .entry(actor_id)
            .or_default()
            .entry((skill_code, is_hot))
            .or_default();
        e.total_heal += amount;
        e.tick_count += 1;
    }

    pub fn get_heal_snapshot(&self) -> HashMap<i32, HashMap<(i32, bool), HealSkillData>> {
        self.inner.read().heal_storage.clone()
    }

    pub fn get_mob_data(&self) -> HashMap<i32, i32> {
        self.inner.read().mob_storage.clone()
    }

    pub fn set_current_target(&self, target: i32) {
        self.inner.write().current_target = target;
    }

    pub fn current_target(&self) -> i32 {
        self.inner.read().current_target
    }

    /// Get a snapshot of all target combat aggregates.
    /// This is cheap: clones a small map of aggregates, not raw packets.
    pub fn get_combat_snapshot(&self) -> HashMap<i32, TargetCombatData> {
        self.inner.read().target_combat.clone()
    }

    /// Like `get_combat_snapshot` but without per-skill `hit_timestamps`.
    /// `hit_timestamps` grows unbounded over a fight and is only needed by
    /// `get_target_details`. The 500ms hot paths (`get_dps`,
    /// `get_details_context`, boss auto-save) never read it, so this keeps
    /// their per-tick clone cost flat over fight duration instead of growing
    /// linearly — the root cause of the long-fight FPS drops.
    pub fn get_combat_snapshot_light(&self) -> HashMap<i32, TargetCombatData> {
        let inner = self.inner.read();
        inner
            .target_combat
            .iter()
            .map(|(&tid, td)| {
                let actors = td
                    .actors
                    .iter()
                    .map(|(&aid, ad)| {
                        let skills = ad
                            .skills
                            .iter()
                            .map(|(&k, sd)| (k, sd.clone_light()))
                            .collect();
                        (
                            aid,
                            ActorCombatData {
                                total_damage: ad.total_damage,
                                party_heal: ad.party_heal,
                                regen: ad.regen,
                                damage_received: ad.damage_received,
                                hits_received: ad.hits_received,
                                last_damage_time: ad.last_damage_time,
                                job: ad.job,
                                skills,
                            },
                        )
                    })
                    .collect();
                (
                    tid,
                    TargetCombatData {
                        target_id: td.target_id,
                        total_damage: td.total_damage,
                        first_damage_time: td.first_damage_time,
                        last_damage_time: td.last_damage_time,
                        last_packet_id: td.last_packet_id,
                        actors,
                    },
                )
            })
            .collect()
    }

    pub fn flush(&self) {
        let mut inner = self.inner.write();
        inner.target_combat.clear();
        inner.actor_jobs.clear();
        inner.summon_storage.clear();
        inner.known_player_ids.clear();
        inner.confirmed_summon_ids.clear();
        inner.summon_spawn_ids.clear();
        inner.actor_power_scalars.clear();
        inner.hostile_target_ids.clear();
        inner.dead_entity_ids.clear();
        inner.has_boss_in_segment = false;
        inner.mob_hp_data.clear();
        inner.mob_current_hp.clear();
        inner.heal_storage.clear();
        inner.current_target = 0;
    }

    /// Clear only the per-segment combat/damage aggregates, preserving player
    /// identity: nicknames, known-player ids, summon ownership, and job classes.
    /// Used on a zone-change / in-instance-teleport reset so the meter starts on
    /// clean numbers without dropping who your party and you are.
    pub fn flush_combat_only(&self) {
        let mut inner = self.inner.write();
        inner.target_combat.clear();
        inner.hostile_target_ids.clear();
        inner.dead_entity_ids.clear();
        inner.has_boss_in_segment = false;
        inner.mob_hp_data.clear();
        inner.mob_current_hp.clear();
        inner.heal_storage.clear();
        inner.current_target = 0;
    }

    pub fn reset_nicknames(&self) {
        let mut inner = self.inner.write();
        inner.nickname_storage.clear();
        inner.pending_nicknames.clear();
        inner.authoritative_name_ids.clear();
        let permanent: Vec<(i32, String)> = inner.permanent_nicknames.iter().map(|(&k, v)| (k, v.clone())).collect();
        for (uid, nick) in permanent {
            inner.nickname_storage.insert(uid, nick);
        }
    }
}

/// Gate for lower-confidence nickname bindings (see `append_nickname`).
fn fuzzy_bind_allowed(inner: &Inner, uid: i32, nickname: &str) -> bool {
    // (1) The id must already be a real entity. A counter/terminator-derived
    // junk id (e.g. `0E 00 36 <counter>`) never appears in combat, is never a
    // known player/summon, was never spawned, and has no name yet — so it is
    // rejected here, and can no longer evict a correct spawn name.
    let is_real_entity = inner.nickname_storage.contains_key(&uid)
        || inner.known_player_ids.contains(&uid)
        || inner.authoritative_name_ids.contains(&uid)
        || inner.summon_storage.contains_key(&uid)
        || inner.target_combat.contains_key(&uid)
        || inner
            .target_combat
            .values()
            .any(|t| t.actors.contains_key(&uid));
    if !is_real_entity {
        return false;
    }
    // (2) Don't let a fuzzy source steal a name an authoritative source already
    // bound to a different id.
    for (&id, name) in &inner.nickname_storage {
        if id != uid && name == nickname && inner.authoritative_name_ids.contains(&id) {
            return false;
        }
    }
    // (3) Don't let a fuzzy source rename an id the protocol already named. The
    // spawn packets carry the owner's LEGION name a few fields past their
    // character name, and the loose scanners happily bind that to the player —
    // which is how a live capture ended up showing a legion ("BaroqueWorks")
    // where a Ranger's name should have been.
    if inner.authoritative_name_ids.contains(&uid)
        && inner.nickname_storage.get(&uid).is_some_and(|n| n != nickname)
    {
        return false;
    }
    true
}

fn has_cjk(s: &str) -> bool {
    s.chars().any(|ch| {
        let cp = ch as u32;
        (0x4E00..=0x9FFF).contains(&cp) || (0xAC00..=0xD7AF).contains(&cp)
        || (0x3400..=0x4DBF).contains(&cp) || (0x20000..=0x2A6DF).contains(&cp)
        || (0x1100..=0x11FF).contains(&cp)
    })
}

fn append_nickname_inner(inner: &mut Inner, uid: i32, nickname: &str) {
    append_nickname_inner_with_force(inner, uid, nickname, false);
}

fn append_nickname_inner_with_force(inner: &mut Inner, uid: i32, nickname: &str, force: bool) {
    let existing = inner.nickname_storage.get(&uid);
    if let Some(existing) = existing {
        if existing == nickname {
            if let Some(ref local_name) = inner.local_character_name {
                if local_name.trim() == nickname.trim() {
                    inner.local_player_id = Some(uid as i64);
                }
            }
            return;
        }
        if !force {
            // Don't replace a CJK name with a shorter ASCII-only name (likely false positive)
            let existing_cjk = has_cjk(existing);
            let new_cjk = has_cjk(nickname);
            if existing_cjk && !new_cjk && nickname.len() < existing.len() {
                tracing::debug!("Nickname: keeping CJK '{}' for {}, rejecting ASCII '{}'", existing, uid, nickname);
                return;
            }
            // Don't replace a longer name with a short ASCII-only name (2-byte rule generalized)
            if !new_cjk && nickname.as_bytes().len() <= 5 && existing.as_bytes().len() > nickname.as_bytes().len() {
                tracing::debug!("Nickname: keeping '{}' for {}, rejecting shorter '{}'", existing, uid, nickname);
                return;
            }
        }
        tracing::trace!("Nickname: replacing '{}' with '{}' for {}{}",
            existing, nickname, uid, if force { " (forced)" } else { "" });
    } else {
        tracing::trace!("Nickname: setting '{}' for {}{}",
            nickname, uid, if force { " (forced)" } else { "" });
    }

    // Name eviction: character names are unique per server, so if this name
    // already belongs to a different entity ID, that old ID is stale (zone change).
    // Evict the old entity's name, player status, and summon mappings regardless
    // of whether the old entity was ever classified as a player.
    let evicted_ids: Vec<i32> = inner.nickname_storage.iter()
        .filter(|&(&old_id, old_name)| old_name == nickname && old_id != uid)
        .map(|(&old_id, _)| old_id)
        .collect();
    for old_id in evicted_ids {
        tracing::debug!("Name eviction: '{}' moved from entity {} to {}", nickname, old_id, uid);
        inner.nickname_storage.remove(&old_id);
        inner.known_player_ids.remove(&old_id);
        inner.authoritative_name_ids.remove(&old_id);
        inner.pending_nicknames.remove(&old_id);
        // Remove summon mappings pointing to the stale owner
        inner.summon_storage.retain(|_, &mut owner| owner != old_id);
        // Also scrub the stale entity's damage from all target aggregates
        for target_data in inner.target_combat.values_mut() {
            if let Some(actor_data) = target_data.actors.remove(&old_id) {
                target_data.total_damage -= actor_data.total_damage;
            }
        }
    }

    inner.nickname_storage.insert(uid, nickname.to_string());

    if !inner.confirmed_summon_ids.contains(&uid) {
        inner.summon_storage.remove(&uid);
    }

    if !inner.confirmed_summon_ids.contains(&uid) {
        let is_new = inner.known_player_ids.insert(uid);
        if is_new {
            purge_friendly_damage(inner, uid);
        }
    }

    if let Some(ref local_name) = inner.local_character_name {
        if local_name.trim() == nickname.trim() {
            inner.local_player_id = Some(uid as i64);
        }
    }
}

fn apply_pending_nickname(inner: &mut Inner, uid: i32) {
    if inner.nickname_storage.contains_key(&uid) { return; }
    if let Some(pending) = inner.pending_nicknames.remove(&uid) {
        append_nickname_inner(inner, uid, &pending);
    }
}

fn is_friendly_action(inner: &Inner, actor_id: i32, target_id: i32) -> bool {
    let resolved_actor = summon_resolver::resolve(actor_id, &inner.summon_storage);
    let resolved_target = summon_resolver::resolve(target_id, &inner.summon_storage);
    inner.known_player_ids.contains(&resolved_actor) && inner.known_player_ids.contains(&resolved_target)
}

/// Remove friendly-fire damage from aggregates when a new player is identified.
fn purge_friendly_damage(inner: &mut Inner, _uid: i32) {
    let mut to_remove: Vec<(i32, Vec<i32>)> = Vec::new();

    for (&target_id, target_data) in &inner.target_combat {
        let mut actors_to_remove = Vec::new();
        for &actor_id in target_data.actors.keys() {
            if is_friendly_action(inner, actor_id, target_id) {
                actors_to_remove.push(actor_id);
            }
        }
        if !actors_to_remove.is_empty() {
            to_remove.push((target_id, actors_to_remove));
        }
    }

    for (target_id, actor_ids) in to_remove {
        if let Some(target_data) = inner.target_combat.get_mut(&target_id) {
            for actor_id in actor_ids {
                if let Some(actor_data) = target_data.actors.remove(&actor_id) {
                    target_data.total_damage -= actor_data.total_damage;
                }
            }
            if target_data.actors.is_empty() {
                inner.target_combat.remove(&target_id);
            }
        }
    }
}

pub fn is_player_skill(skill_code: i32) -> bool {
    // Class skills: 11M-19M (post-divide 110K-190K, encodes class in first 2 digits)
    // Alternate band: 3M-3.99M (post-divide 30K-39.9K)
    // Basic/special attacks: 100K-199K (post-divide 1K-1.9K)
    (11_000_000..=19_999_999).contains(&skill_code)
        || (3_000_000..=3_999_999).contains(&skill_code)
        || (100_000..=199_999).contains(&skill_code)
}
