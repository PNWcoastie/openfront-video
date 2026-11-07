//! The game state and the tick loop (engine GameImpl / PlayerImpl / GameRunner
//! and the executions), as far as the port goes: the spawn phase, income,
//! relations, embargoes, land attacks, conquering players, the territory
//! checks and tribe (bot) AI, without boats, units, alliances or fallout.

use std::collections::{HashMap, HashSet};

use crate::fxhash::FxHashMap;

use crate::detmath::{js_min, log, pow, sigmoid, within};
use crate::heap::FlatBinaryHeap;
use crate::js::{is_integer, simple_hash};
use crate::map::{eucl_dist_center, GameMap, Terrain};
use crate::rng::PseudoRandom;
use crate::tileset::TileSet;
use crate::water::WaterPathfinder;

mod alliances;
mod doomsday;
mod namebox;
mod nation;
mod nation_attack;
mod nation_nukes;
mod nation_structures;
mod nation_warships;
mod nukes;
mod ports_trade;
mod rail;
mod severed;
mod structures;
mod units;
mod warships;

pub use alliances::*;
pub use doomsday::DoomsdayExec;
pub use namebox::*;
pub use nation::{NationExec, NationInfo};
pub use nation_attack::{BoatRoute, EmojiExec};
pub use nation_nukes::*;
pub use nation_structures::*;
pub use nukes::{
    ATOM_BOMB, Curve, HYDROGEN_BOMB, MIRV_WARHEAD, MirvExec, NukeExec, NukeState, SAM_MISSILE,
    SamExec, SamMissileExec, SamState,
};
pub use ports_trade::*;
pub use rail::*;
pub use severed::*;
pub use structures::*;
pub use units::{Step, Stepper, TransportShipExec};
pub use warships::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlayerType {
    Bot,
    Human,
    Nation,
}

impl PlayerType {
    pub fn as_str(self) -> &'static str {
        match self {
            PlayerType::Bot => "BOT",
            PlayerType::Human => "HUMAN",
            PlayerType::Nation => "NATION",
        }
    }
}

#[derive(Clone, Debug)]
pub struct PlayerInfo {
    pub name: String,
    pub player_type: PlayerType,
    pub client_id: Option<String>,
    pub id: String,
    pub is_lobby_creator: bool,
    pub clan_tag: Option<String>,
    pub friends: Vec<String>,
    pub team_index: Option<usize>,
}

pub struct Embargo {
    pub target: usize,
    pub created_at: u32,
    pub is_temporary: bool,
}

pub struct Player {
    pub small_id: u16,
    pub info: PlayerInfo,
    pub team: Option<String>,
    pub troops: i64,
    pub gold: i64,
    pub tiles: TileSet,
    pub border_tiles: TileSet,
    pub spawn_tile: Option<u32>,
    pub is_disconnected: bool,
    pub last_tile_change: u32,
    pub tile_change_version: u64,
    pub random: PseudoRandom,
    /// Map<Player, number> in insertion order.
    pub relations: Vec<(usize, f64)>,
    /// Map<PlayerID, Embargo> in insertion order.
    pub embargoes: Vec<Embargo>,
    pub outgoing: Vec<usize>,
    pub incoming: Vec<usize>,
    pub marked_traitor_tick: i64,
    /// Stats: troops sent in attacks (only kept for humans, like StatsImpl).
    pub attacks_sent: i64,
    /// Units in creation order (indexes into Game::units).
    pub units: Vec<usize>,
    /// Alliances in the order they formed (indexes into Game::alliances).
    pub alliances: Vec<usize>,
    pub past_outgoing_requests: Vec<usize>,
    pub sent_donations: Vec<(usize, u32)>,
    /// PlayerImpl.largestClusterBoundingBox: [minX, minY, maxX, maxY].
    pub largest_cluster_box: Option<[i64; 4]>,
    /// Emojis sent: (recipient, None for everyone; tick).
    pub outgoing_emojis: Vec<(Option<usize>, u32)>,
    /// Players targeted: (tick, target).
    pub targets: Vec<(u32, usize)>,
    pub units_constructed: HashMap<&'static str, u32>,
    pub last_delete_unit_tick: i64,
    pub last_embargo_all_tick: i64,
    /// Lifetime gold income (PlayerImpl.goldEarned): everything addGold added.
    pub gold_earned: i64,
    /// The tick the Doomsday Clock marked this player (-1: not marked).
    pub marked_doomsday_tick: i64,
}

impl Player {
    pub fn has_spawned(&self) -> bool {
        self.spawn_tile.is_some()
    }
    pub fn is_alive(&self) -> bool {
        self.tiles.len() > 0
    }
    pub fn troops_f(&self) -> f64 {
        self.troops as f64
    }
    fn relation_value(&self, other: usize) -> f64 {
        self.relations.iter().find(|(p, _)| *p == other).map(|(_, v)| *v).unwrap_or(0.0)
    }
    fn set_relation(&mut self, other: usize, v: f64) {
        match self.relations.iter_mut().find(|(p, _)| *p == other) {
            Some(e) => e.1 = v,
            None => self.relations.push((other, v)),
        }
    }
}

/// A unit (UnitImpl); only transport ships so far.
pub struct Unit {
    pub id: u32,
    pub type_name: &'static str,
    pub owner: usize,
    pub tile: u32,
    pub last_tile: u32,
    pub troops: f64,
    pub target_tile: Option<u32>,
    pub active: bool,
    pub retreating: bool,
    pub level: u32,
    pub under_construction: bool,
    pub deletion_at: Option<u32>,
    pub has_train_station: bool,
    pub health: i64,
    pub warship: Option<WarshipState>,
    pub target_unit: Option<usize>,
    pub last_safe_from_pirates: u32,
    pub grid_seq: u64,
    pub missile_timers: Vec<u32>,
    pub nuke: Option<Box<NukeState>>,
    pub targetable: bool,
    pub sam_state: Option<SamState>,
    pub train_engine: bool,
    /// The enemy that destroyed it (wasDestroyedByEnemy / destroyer).
    pub destroyer: Option<usize>,
    /// A MIRV's target player captured at launch (Unit.targetPlayer).
    pub mirv_target: Option<usize>,
    /// Unit.reachedTarget: a nuke that detonated (not intercepted).
    pub reached_target: bool,
}

pub const TRANSPORT_SHIP: &str = "Transport";

/// An attack in progress (AttackImpl).
pub struct Attack {
    pub id: String,
    pub target: Option<usize>,
    pub attacker: usize,
    pub troops: f64,
    pub source_tile: Option<u32>,
    /// Insertion-ordered, like the engine's Set: nations read it in order.
    border: TileSet,
    pub border_size: i64,
    pub active: bool,
    pub retreating: bool,
    pub retreated: bool,
}

/// Game settings this far into the port (EngineConfig).
pub struct Config {
    pub game_id: String,
    pub is_singleplayer: bool,
    pub is_team_mode: bool,
    pub random_spawn: bool,
    pub infinite_troops: bool,
    pub starting_gold: i64,
    pub spawn_immunity_duration: u32,
    pub disable_alliances: bool,
    pub gold_multiplier: f64,
    pub difficulty: String,
    pub alliance_duration: u32,
    pub donate_gold: bool,
    pub donate_troops: bool,
    pub overtime_enabled: bool,
    pub overtime_start_minutes: f64,
    pub max_timer_minutes: Option<f64>,
    /// The Doomsday Clock's speed preset, when it is on.
    pub doomsday_speed: Option<String>,
    /// Humans on one team, nations on the other.
    pub humans_vs_nations: bool,
    pub is_public: bool,
}

impl Config {
    pub fn num_spawn_phase_turns(&self) -> u32 {
        if self.is_singleplayer {
            100
        } else if self.random_spawn {
            150
        } else {
            200
        }
    }
    pub fn start_manpower(&self, info: &PlayerInfo) -> i64 {
        match info.player_type {
            PlayerType::Bot => 10_000,
            PlayerType::Nation => match self.difficulty.as_str() {
                "Easy" => 12_500,
                "Medium" => 18_750,
                "Hard" => 25_000,
                _ => 31_250,
            },
            PlayerType::Human => {
                if self.infinite_troops {
                    1_000_000
                } else {
                    25_000
                }
            }
        }
    }
    pub fn starting_gold(&self, info: &PlayerInfo) -> i64 {
        if info.player_type == PlayerType::Bot {
            0
        } else {
            self.starting_gold
        }
    }
}

// ── Executions ──

pub struct SpawnExec {
    random: PseudoRandom,
    active: bool,
    info: PlayerInfo,
    tile: Option<f64>,
    from_intent: bool,
    queued_during_spawn_phase: bool,
}

pub struct PlayerExec {
    player: usize,
    last_calc: i64,
    active: bool,
}

pub struct TribeExec {
    player: usize,
    active: bool,
    random: PseudoRandom,
    neighbors_terra_nullius: bool,
    has_behavior: bool,
    attack_rate: f64,
    attack_tick: f64,
    trigger_ratio: f64,
    reserve_ratio: f64,
    expand_ratio: f64,
}

pub struct AttackExec {
    active: bool,
    to_conquer: FlatBinaryHeap,
    random: PseudoRandom,
    /// None: terra nullius (an id of null).
    target_id: Option<String>,
    target: Option<usize>,
    owner: usize,
    start_troops: Option<f64>,
    source_tile: Option<u32>,
    remove_troops: bool,
    attack: Option<usize>,
    owner_small: u16,
    target_small: u16,
}

pub enum Exec {
    SpawnTimer,
    Spawn(Box<SpawnExec>),
    WinCheck { active: bool },
    Doomsday(Box<DoomsdayExec>),
    RecomputeRailCluster,
    MarkDisconnected { player: usize, disconnected: bool },
    Player(Box<PlayerExec>),
    Tribe(Box<TribeExec>),
    Attack(Box<AttackExec>),
    TransportShip(Box<TransportShipExec>),
    AllianceRequest(Box<AllianceRequestExec>),
    Construction(Box<ConstructionExec>),
    Port(Box<PortExec>),
    DefensePost { unit: usize, active: bool },
    TradeShip(Box<TradeShipExec>),
    City { unit: usize, station_created: bool, active: bool },
    Factory { unit: usize, station_created: bool, active: bool },
    TrainStation(Box<TrainStationExec>),
    Train(Box<TrainExec>),
    Nation(Box<NationExec>),
    AllianceExtension { from: usize, to_id: String },
    Sam(Box<SamExec>),
    Nuke(Box<NukeExec>),
    Mirv(Box<MirvExec>),
    SamMissile(Box<SamMissileExec>),
    Silo { unit: usize, active: bool },
    UpgradeStructure { player: usize, unit_id: u32, amount: u32 },
    EmbargoAll { player: usize, start: bool },
    Donate(Box<DonateExec>),
    Warship(Box<WarshipExec>),
    Shell(Box<ShellExec>),
    MoveWarship { owner: usize, ids: Vec<u32>, position: f64 },
    Emoji(Box<EmojiExec>),
    BoatRetreat { active: bool, player: usize, unit_id: u32 },
    TargetPlayer { active: bool, requestor: usize, target_id: String, target: Option<usize> },
    Embargo { active: bool, player: usize, target_id: String, start: bool, target: Option<usize> },
    /// Quick chat only sends messages.
    QuickChat { active: bool, recipient_id: String },
    Retreat { active: bool, player: usize, attack_id: String, ordered: bool, start_tick: u32 },
    DeleteUnit { active: bool, player: usize, unit_id: u32, unit: Option<usize> },
    AllianceReject { requestor_id: String, recipient: usize },
    BreakAlliance { active: bool, requestor: usize, recipient_id: String, recipient: Option<usize> },
    NoOp,
}

impl Exec {
    /// The TypeScript class name, for the comparison harness.
    pub fn name(&self) -> &'static str {
        match self {
            Exec::SpawnTimer => "SpawnTimerExecution",
            Exec::Spawn(_) => "SpawnExecution",
            Exec::WinCheck { .. } => "WinCheckExecution",
            Exec::Doomsday(_) => "DoomsdayClockExecution",
            Exec::RecomputeRailCluster => "RecomputeRailClusterExecution",
            Exec::MarkDisconnected { .. } => "MarkDisconnectedExecution",
            Exec::Player(_) => "PlayerExecution",
            Exec::Tribe(_) => "TribeExecution",
            Exec::Attack(_) => "AttackExecution",
            Exec::TransportShip(_) => "TransportShipExecution",
            Exec::AllianceRequest(_) => "AllianceRequestExecution",
            Exec::Construction(_) => "ConstructionExecution",
            Exec::Port(_) => "PortExecution",
            Exec::DefensePost { .. } => "DefensePostExecution",
            Exec::TradeShip(_) => "TradeShipExecution",
            Exec::City { .. } => "CityExecution",
            Exec::Factory { .. } => "FactoryExecution",
            Exec::TrainStation(_) => "TrainStationExecution",
            Exec::Train(_) => "TrainExecution",
            Exec::Nation(_) => "NationExecution",
            Exec::AllianceExtension { .. } => "AllianceExtensionExecution",
            Exec::Sam(_) => "SAMLauncherExecution",
            Exec::Nuke(_) => "NukeExecution",
            Exec::Mirv(_) => "MirvExecution",
            Exec::SamMissile(_) => "SAMMissileExecution",
            Exec::Silo { .. } => "MissileSiloExecution",
            Exec::UpgradeStructure { .. } => "UpgradeStructureExecution",
            Exec::EmbargoAll { .. } => "EmbargoAllExecution",
            Exec::Donate(d) => if d.gold { "DonateGoldExecution" } else { "DonateTroopsExecution" },
            Exec::Warship(_) => "WarshipExecution",
            Exec::Shell(_) => "ShellExecution",
            Exec::MoveWarship { .. } => "MoveWarshipExecution",
            Exec::Emoji(_) => "EmojiExecution",
            Exec::BoatRetreat { .. } => "BoatRetreatExecution",
            Exec::TargetPlayer { .. } => "TargetPlayerExecution",
            Exec::Embargo { .. } => "EmbargoExecution",
            Exec::QuickChat { .. } => "QuickChatExecution",
            Exec::Retreat { .. } => "RetreatExecution",
            Exec::DeleteUnit { .. } => "DeleteUnitExecution",
            Exec::AllianceReject { .. } => "AllianceRejectExecution",
            Exec::BreakAlliance { .. } => "BreakAllianceExecution",
            Exec::NoOp => "NoOpExecution",
        }
    }

    fn active_during_spawn_phase(&self) -> bool {
        matches!(self, Exec::SpawnTimer | Exec::Spawn(_) | Exec::Nation(_))
    }

    fn is_active(&self, game: &Game) -> bool {
        match self {
            Exec::SpawnTimer => game.in_spawn_phase(),
            Exec::Spawn(s) => s.active,
            Exec::NoOp | Exec::MarkDisconnected { .. } => false,
            Exec::WinCheck { active } => *active,
            Exec::Doomsday(d) => d.active,
            Exec::RecomputeRailCluster => true,
            Exec::Player(p) => p.active,
            Exec::Tribe(t) => t.active,
            Exec::Attack(a) => a.active,
            Exec::TransportShip(t) => t.active,
            Exec::AllianceRequest(a) => a.active,
            Exec::Construction(c) => c.active,
            Exec::Port(p) => p.active,
            Exec::DefensePost { active, .. } => *active,
            Exec::TradeShip(t) => t.active,
            Exec::City { active, .. } => *active,
            Exec::Factory { active, .. } => *active,
            Exec::TrainStation(t) => t.active,
            Exec::Train(t) => t.active,
            Exec::Nation(n) => n.active,
            Exec::AllianceExtension { .. } => false,
            Exec::Silo { active, .. } => *active,
            Exec::Sam(s) => s.active,
            Exec::Nuke(n) => n.active && !n.warhead_id.is_some_and(|w| game.warhead_cancelled[w]),
            Exec::Mirv(m) => m.active,
            Exec::SamMissile(m) => m.active,
            Exec::UpgradeStructure { .. } | Exec::EmbargoAll { .. } => false,
            Exec::Donate(d) => d.active,
            Exec::Warship(w) => w.warship.is_some_and(|u| game.units[u].active),
            Exec::Shell(s) => s.active,
            Exec::MoveWarship { .. } => false,
            Exec::Retreat { active, .. } | Exec::QuickChat { active, .. } => *active,
            Exec::Emoji(e) => e.active,
            Exec::BoatRetreat { active, .. } | Exec::TargetPlayer { active, .. } | Exec::Embargo { active, .. } => *active,
            Exec::DeleteUnit { active, .. } => *active,
            Exec::AllianceReject { .. } => false,
            Exec::BreakAlliance { active, .. } => *active,
        }
    }
}

/// Where a nearby() entry points: a player, or terra nullius.
pub type Neighbor = Option<usize>;

const TICKS_PER_CLUSTER_CALC: i64 = 20;
const CLUSTER_SURROUNDED: u8 = 1;
const CLUSTER_SEVERED_CANDIDATE: u8 = 2;
const CLUSTER_OPEN: u8 = 4;

pub struct Game {
    pub config: Config,
    pub map: GameMap,
    pub players: Vec<Player>,
    players_by_id: HashMap<String, usize>,
    pub player_teams: Vec<String>,
    /// Map manifest teamGameSpawnAreas: per team count, one [x, y, width, height] per team.
    pub team_spawn_areas: Vec<(String, Vec<[i64; 4]>)>,
    pub ticks: u32,
    start_tick: Option<u32>,
    pub execs: Vec<Exec>,
    pub uninit_execs: Vec<Exec>,
    pub attacks: Vec<Attack>,
    pub units: Vec<Unit>,
    next_unit_id: u32,
    pub water: WaterPathfinder,
    rail: RailNetwork,
    train_units: u32,
    grid_seq: u64,
    mirvs_launched: u32,
    /// Game.nationMirvTargets: player -> last tick a nation MIRVed them.
    nation_mirv_targets: FxHashMap<usize, u32>,
    /// Every unit ever built, by type (inactive ones are skipped on read).
    units_by_type: FxHashMap<&'static str, Vec<usize>>,
    /// Live units (except trains) per 100x100 cell, like UnitGrid.
    cells: Vec<Vec<usize>>,
    shared_water: SharedWaterCache,
    /// Minimap water tiles per water component.
    water_sizes: Vec<u32>,
    warhead_units: Vec<Option<usize>>,
    warhead_cancelled: Vec<bool>,
    pub winner: Option<String>,
    /// ENGINE_RS_PROFILE: time spent per execution type.
    pub profile: Option<HashMap<&'static str, std::time::Duration>>,
    pub requests: Vec<AllianceRequest>,
    /// Pending requests in creation order (GameImpl.allianceRequests).
    pub alliance_requests: Vec<usize>,
    pub alliances: Vec<Alliance>,
    next_alliance_id: u32,
    // TileTraversalScratch, shared by every traversal.
    visited: Vec<u32>,
    cluster_index_map: Vec<i32>,
    gen: u32,
    tile_slot: Vec<u32>,
    border_slot: Vec<u32>,
    dfs_stack: Vec<u32>,
    territory_sigmoid_lut: Vec<f64>,
}

impl Game {
    pub fn new(config: Config, map: GameMap, mini_map: GameMap, humans: Vec<PlayerInfo>, nations: Vec<PlayerInfo>, teams: Option<TeamSetup>) -> Game {
        let n = map.num_tiles() as usize;
        let num_cells = (((map.width + 99) / 100) * ((map.height + 99) / 100)) as usize;
        let log_mid = log(300_000.0);
        let territory_sigmoid_lut: Vec<f64> = (0..=map.num_land_tiles as usize).map(|cnt| sigmoid(log(cnt as f64), 2.5, log_mid)).collect();
        let mut g = Game {
            config,
            map,
            players: Vec::new(),
            players_by_id: HashMap::new(),
            player_teams: Vec::new(),
            team_spawn_areas: Vec::new(),
            ticks: 0,
            start_tick: None,
            execs: Vec::new(),
            uninit_execs: Vec::new(),
            attacks: Vec::new(),
            visited: vec![0; n],
            cluster_index_map: vec![0; n],
            units: Vec::new(),
            next_unit_id: 1,
            water: WaterPathfinder::new(mini_map),
            rail: RailNetwork::default(),
            train_units: 0,
            grid_seq: 0,
            mirvs_launched: 0,
            nation_mirv_targets: FxHashMap::default(),
            units_by_type: FxHashMap::default(),
            cells: vec![Vec::new(); num_cells],
            shared_water: SharedWaterCache::default(),
            water_sizes: Vec::new(),
            warhead_units: Vec::new(),
            warhead_cancelled: Vec::new(),
            winner: None,
            profile: std::env::var("ENGINE_RS_PROFILE").ok().map(|_| HashMap::new()),
            requests: Vec::new(),
            alliance_requests: Vec::new(),
            alliances: Vec::new(),
            next_alliance_id: 0,
            gen: 0,
            tile_slot: vec![u32::MAX; n],
            border_slot: vec![u32::MAX; n],
            dfs_stack: Vec::new(),
            territory_sigmoid_lut,
        };
        for t in 0..g.water.mini.num_tiles() {
            if g.water.mini.is_water(t) {
                let c = g.water.component_id(t) as usize;
                if g.water_sizes.len() <= c {
                    g.water_sizes.resize(c + 1, 0);
                }
                g.water_sizes[c] += 1;
            }
        }
        if g.config.is_team_mode {
            let setup = teams.expect("team setup");
            if setup.humans_vs_nations {
                g.player_teams = vec!["Humans".to_string(), "Nations".to_string()];
                for h in humans {
                    g.add_player(h, Some("Humans".to_string()));
                }
                for n in nations {
                    g.add_player(n, Some("Nations".to_string()));
                }
            } else {
                // Humans and nations are dealt into the teams together.
                g.player_teams = resolve_teams_list(setup.count);
                let all: Vec<PlayerInfo> = humans.into_iter().chain(nations).collect();
                let max_team_size = all.len().div_ceil(g.player_teams.len());
                let teams = g.player_teams.clone();
                for (info, team) in assign_teams(&all, &teams, setup.duos_trios_quads, max_team_size) {
                    if let Some(team) = team {
                        g.add_player(info, Some(team));
                    }
                }
            }
        } else {
            for h in humans.into_iter().chain(nations) {
                g.add_player(h, None);
            }
        }
        g
    }

    pub fn in_spawn_phase(&self) -> bool {
        self.start_tick.is_none()
    }

    fn end_spawn_phase(&mut self) {
        if self.start_tick.is_none() {
            self.start_tick = Some(self.ticks);
        }
    }

    fn ticks_since_start(&self) -> u32 {
        match self.start_tick {
            None => 0,
            Some(s) => self.ticks.saturating_sub(s),
        }
    }

    fn is_spawn_immunity_active(&self) -> bool {
        self.in_spawn_phase() || self.ticks_since_start() < self.config.spawn_immunity_duration
    }

    pub fn add_player(&mut self, info: PlayerInfo, team: Option<String>) -> usize {
        let team = team.or_else(|| self.maybe_assign_team(&info));
        let idx = self.players.len();
        let player = Player {
            small_id: (idx + 1) as u16,
            troops: self.config.start_manpower(&info),
            gold: self.config.starting_gold(&info),
            random: PseudoRandom::new(simple_hash(&info.id)),
            info,
            team,
            tiles: TileSet::default(),
            border_tiles: TileSet::default(),
            spawn_tile: None,
            is_disconnected: false,
            last_tile_change: 0,
            tile_change_version: 0,
            relations: Vec::new(),
            embargoes: Vec::new(),
            outgoing: Vec::new(),
            incoming: Vec::new(),
            marked_traitor_tick: -1,
            attacks_sent: 0,
            units: Vec::new(),
            alliances: Vec::new(),
            past_outgoing_requests: Vec::new(),
            sent_donations: Vec::new(),
            largest_cluster_box: None,
            outgoing_emojis: Vec::new(),
            targets: Vec::new(),
            units_constructed: HashMap::new(),
            last_delete_unit_tick: -1,
            last_embargo_all_tick: -1,
            gold_earned: 0,
            marked_doomsday_tick: -1,
        };
        self.players_by_id.insert(player.info.id.clone(), idx);
        self.players.push(player);
        idx
    }

    fn maybe_assign_team(&self, info: &PlayerInfo) -> Option<String> {
        if !self.config.is_team_mode {
            return None;
        }
        if info.player_type == PlayerType::Bot {
            return Some("Bot".to_string());
        }
        let rand = simple_hash(&info.id) as usize;
        Some(self.player_teams[rand % self.player_teams.len()].clone())
    }

    pub fn player_by_client_id(&self, client_id: &str) -> Option<usize> {
        self.players.iter().position(|p| p.info.client_id.as_deref() == Some(client_id))
    }

    pub fn player_index(&self, id: &str) -> Option<usize> {
        self.players_by_id.get(id).copied()
    }

    fn owner_index(&self, tile: u32) -> Option<usize> {
        match self.map.owner_id(tile) {
            0 => None,
            id => Some(id as usize - 1),
        }
    }

    fn small(&self, p: Neighbor) -> u16 {
        p.map(|i| self.players[i].small_id).unwrap_or(0)
    }

    // ── Players: teams, friendship, troops, gold ──

    /// GameImpl.teamSpawnArea
    fn team_spawn_area(&self, p: usize) -> Option<[i64; 4]> {
        let team = self.players[p].team.as_ref()?;
        let key = self.player_teams.len().to_string();
        let areas = &self.team_spawn_areas.iter().find(|a| a.0 == key)?.1;
        let i = self.player_teams.iter().position(|t| t == team)?;
        areas.get(i).copied()
    }

    fn is_on_same_team(&self, a: usize, b: usize) -> bool {
        if a == b {
            return false;
        }
        match (&self.players[a].team, &self.players[b].team) {
            (Some(ta), Some(tb)) => ta != "Bot" && tb != "Bot" && ta == tb,
            _ => false,
        }
    }

    fn is_allied_with(&self, a: usize, b: usize) -> bool {
        if a == b {
            return false;
        }
        self.alliance_with(a, b).is_some()
    }

    /// PlayerImpl.isFriendly: `a` asks about `other`.
    fn is_friendly(&self, a: usize, other: usize, treat_afk_friendly: bool) -> bool {
        if other == a {
            return true;
        }
        if self.players[other].is_disconnected && !treat_afk_friendly {
            return false;
        }
        self.is_on_same_team(a, other) || self.is_allied_with(a, other)
    }

    fn is_traitor(&self, p: usize) -> bool {
        let marked = self.players[p].marked_traitor_tick;
        if marked < 0 {
            return false;
        }
        let elapsed = self.ticks as i64 - marked;
        300 - elapsed > 0
    }

    fn is_immune(&self, p: usize) -> bool {
        match self.players[p].info.player_type {
            PlayerType::Human => self.is_spawn_immunity_active(),
            PlayerType::Nation => self.in_spawn_phase() || self.ticks_since_start() < 50,
            PlayerType::Bot => false,
        }
    }

    fn can_attack_player(&self, attacker: usize, target: usize) -> bool {
        if self.players[attacker].info.player_type != PlayerType::Human {
            return !self.is_friendly(attacker, target, false);
        }
        !self.is_immune(target) && !self.is_friendly(attacker, target, false)
    }

    fn add_troops(&mut self, p: usize, troops: f64) {
        if troops < 0.0 {
            self.remove_troops(p, -1.0 * troops);
            return;
        }
        self.players[p].troops += to_int(troops);
    }

    /// Returns the troops actually removed.
    fn remove_troops(&mut self, p: usize, troops: f64) -> f64 {
        if troops <= 0.0 {
            return 0.0;
        }
        let to_remove = self.players[p].troops.min(to_int(troops));
        self.players[p].troops -= to_remove;
        to_remove as f64
    }

    fn remove_gold(&mut self, p: usize, amount: i64) -> i64 {
        if amount <= 0 {
            return 0;
        }
        let actual = self.players[p].gold.min(amount);
        self.players[p].gold -= actual;
        actual
    }

    fn update_relation(&mut self, p: usize, other: usize, delta: f64) {
        assert_ne!(p, other, "cannot update relation with self");
        let r = self.players[p].relation_value(other);
        self.players[p].set_relation(other, within(r + delta, -100.0, 100.0));
    }

    fn is_hostile(&self, p: usize, other: usize) -> bool {
        self.players[p].relation_value(other) < -50.0
    }

    fn decay_relations(&mut self, p: usize) {
        for e in self.players[p].relations.iter_mut() {
            let r = e.1;
            let sign = -1.0 * js_sign(r);
            let delta = 0.05;
            let mut r = r + sign * delta;
            if r.abs() < delta * 2.0 {
                r = 0.0;
            }
            e.1 = r;
        }
    }

    fn add_embargo(&mut self, p: usize, other: usize, is_temporary: bool) {
        let ticks = self.ticks;
        let embargoes = &mut self.players[p].embargoes;
        match embargoes.iter_mut().find(|e| e.target == other) {
            Some(e) => {
                if !e.is_temporary {
                    return;
                }
                e.created_at = ticks;
                e.is_temporary = is_temporary;
            }
            None => embargoes.push(Embargo { target: other, created_at: ticks, is_temporary }),
        }
    }

    // ── Config formulas (Config / EngineConfig) ──

    fn max_troops(&self, p: usize) -> f64 {
        self.troop_cap(p, true)
    }

    /// maxTroopsWithoutCities
    fn max_troops_without_cities(&self, p: usize) -> f64 {
        self.troop_cap(p, false)
    }

    fn troop_cap(&self, p: usize, with_cities: bool) -> f64 {
        let pl = &self.players[p];
        let city_troops = if with_cities { self.city_troops(p) } else { 0.0 };
        let max = if pl.info.player_type == PlayerType::Human && self.config.infinite_troops {
            1_000_000_000.0
        } else {
            2.0 * (pow(pl.tiles.len() as f64, 0.6) * 1000.0 + 50000.0) + city_troops
        };
        match pl.info.player_type {
            PlayerType::Bot => max / 3.0,
            PlayerType::Human => max,
            PlayerType::Nation => match self.config.difficulty.as_str() {
                "Easy" => max * 0.5,
                "Medium" => max * 0.75,
                "Hard" => max,
                _ => max * 1.25,
            },
        }
    }

    fn troop_increase_rate(&self, p: usize) -> f64 {
        let max = self.max_troops(p);
        let troops = self.players[p].troops_f();
        let mut to_add = 10.0 + pow(troops, 0.73) / 4.0;
        let ratio = 1.0 - troops / max;
        to_add *= ratio;
        if self.players[p].info.player_type == PlayerType::Bot {
            to_add *= 0.5;
        }
        if self.players[p].info.player_type == PlayerType::Nation {
            to_add *= match self.config.difficulty.as_str() {
                "Easy" => 0.9,
                "Medium" => 0.95,
                "Hard" => 1.0,
                _ => 1.05,
            };
        }
        js_min(troops + to_add, max) - troops
    }

    fn gold_addition_rate(&self, p: usize) -> i64 {
        let base = if self.players[p].info.player_type == PlayerType::Bot { 50.0 } else { 100.0 };
        (base * self.config.gold_multiplier).floor() as i64
    }

    // ── Tiles ──

    /// GameImpl.conquer
    pub fn conquer(&mut self, owner: usize, tile: u32) {
        assert!(self.map.is_land(tile), "cannot conquer water");
        assert!(!self.map.is_impassable(tile), "cannot conquer impassable terrain");
        if let Some(prev) = self.owner_index(tile) {
            let p = &mut self.players[prev];
            p.last_tile_change = self.ticks;
            p.tile_change_version += 1;
            p.tiles.delete_indexed(tile, &mut self.tile_slot);
            p.border_tiles.delete_indexed(tile, &mut self.border_slot);
        }
        let small_id = self.players[owner].small_id;
        self.map.set_owner_id(tile, small_id);
        let p = &mut self.players[owner];
        p.tiles.add_indexed(tile, &mut self.tile_slot);
        p.last_tile_change = self.ticks;
        p.tile_change_version += 1;
        self.update_borders(tile);
        self.map.set_fallout(tile, false);
    }

    /// GameImpl.relinquish
    pub fn relinquish(&mut self, tile: u32) {
        let prev = self.owner_index(tile).expect("cannot relinquish an unowned tile");
        let p = &mut self.players[prev];
        p.last_tile_change = self.ticks;
        p.tile_change_version += 1;
        p.tiles.delete_indexed(tile, &mut self.tile_slot);
        p.border_tiles.delete_indexed(tile, &mut self.border_slot);
        self.map.set_owner_id(tile, 0);
        self.update_borders(tile);
    }

    fn update_borders(&mut self, tile: u32) {
        self.update_border_status(tile);
        let mut nb = [0u32; 4];
        let n = self.map.neighbors4(tile, &mut nb);
        for &t in &nb[..n] {
            self.update_border_status(t);
        }
    }

    fn update_border_status(&mut self, t: u32) {
        let Some(owner) = self.owner_index(t) else { return };
        if self.map.is_border(t) {
            self.players[owner].border_tiles.add_indexed(t, &mut self.border_slot);
        } else {
            self.players[owner].border_tiles.delete_indexed(t, &mut self.border_slot);
        }
    }

    /// GameImpl.hash: summed in float64 in player order, like the engine.
    pub fn hash(&self) -> f64 {
        let mut hash = 1.0;
        for p in &self.players {
            let mut units = 0.0;
            for &u in &p.units {
                let u = &self.units[u];
                units += u.tile as f64 + simple_hash(u.type_name) * u.id as f64;
            }
            hash += simple_hash(&p.info.id) * (p.troops as f64 + p.tiles.len() as f64) + units;
        }
        hash
    }

    /// PlayerImpl.sharesBorderWith
    fn shares_border_with(&self, p: usize, other_small: u16) -> bool {
        let mut nb = [0u32; 4];
        if other_small > 0 {
            let other = other_small as usize - 1;
            if other < self.players.len() && self.players[other].border_tiles.len() < self.players[p].border_tiles.len() {
                let my_small = self.players[p].small_id;
                for border in self.players[other].border_tiles.values() {
                    let n = self.map.neighbors4(border, &mut nb);
                    if nb[..n].iter().any(|&t| self.map.owner_id(t) == my_small) {
                        return true;
                    }
                }
                return false;
            }
        }
        for border in self.players[p].border_tiles.values() {
            let n = self.map.neighbors4(border, &mut nb);
            if nb[..n].iter().any(|&t| self.map.owner_id(t) == other_small) {
                return true;
            }
        }
        false
    }

    /// PlayerImpl.nearby (computeNearby): neighbors over land, then across
    /// narrow water, in first-seen order.
    fn nearby(&self, p: usize) -> Vec<Neighbor> {
        let me = self.players[p].small_id;
        let mut seen: Vec<u16> = Vec::new();
        let mut seen_bits = [false; 1024];
        let mut add = |small: u16, seen: &mut Vec<u16>| {
            let idx = small as usize;
            if idx < seen_bits.len() {
                if !seen_bits[idx] {
                    seen_bits[idx] = true;
                    seen.push(small);
                }
            } else if !seen.contains(&small) {
                seen.push(small);
            }
        };
        let map = &self.map;
        let mut nb = [0u32; 4];
        for border in self.players[p].border_tiles.values() {
            let n = map.neighbors4(border, &mut nb);
            for &t in &nb[..n] {
                if map.is_land(t) && !map.is_impassable(t) {
                    if !map.has_owner(t) && map.has_fallout(t) {
                        continue;
                    }
                    let owner = map.owner_id(t);
                    if owner != me {
                        add(owner, &mut seen);
                    }
                }
            }
        }
        // shoreReachableNeighbors: every 10th shore border tile, 5 tiles out
        // in each direction that starts with water.
        let mut shore_idx = 0;
        let (w, h) = (map.width as i64, map.height as i64);
        for border in self.players[p].border_tiles.values() {
            if !map.is_shore(border) {
                continue;
            }
            let idx = shore_idx;
            shore_idx += 1;
            if idx % 10 != 0 {
                continue;
            }
            let (bx, by) = (map.x(border) as i64, map.y(border) as i64);
            for (dx, dy) in [(0i64, -1i64), (0, 1), (-1, 0), (1, 0)] {
                let (x1, y1) = (bx + dx, by + dy);
                if x1 < 0 || y1 < 0 || x1 >= w || y1 >= h || !map.is_water(map.ref_at(x1 as u32, y1 as u32)) {
                    continue;
                }
                let (nx, ny) = (bx + dx * 5, by + dy * 5);
                if nx < 0 || ny < 0 || nx >= w || ny >= h {
                    continue;
                }
                let tile = map.ref_at(nx as u32, ny as u32);
                if !map.is_land(tile) || map.is_impassable(tile) {
                    continue;
                }
                if !map.has_owner(tile) && map.has_fallout(tile) {
                    continue;
                }
                let owner = map.owner_id(tile);
                if owner != me {
                    add(owner, &mut seen);
                }
            }
        }
        seen.into_iter().map(|s| if s == 0 { None } else { Some(s as usize - 1) }).collect()
    }

    // ── The tick ──

    pub fn add_execution(&mut self, e: Exec) {
        self.uninit_execs.push(e);
    }

    /// GameImpl.executeNextTick
    pub fn execute_next_tick(&mut self) {
        let mut execs = std::mem::take(&mut self.execs);
        for e in execs.iter_mut() {
            if (!self.in_spawn_phase() || e.active_during_spawn_phase()) && e.is_active(self) {
                if self.profile.is_some() {
                    let t0 = std::time::Instant::now();
                    self.tick_exec(e);
                    let d = t0.elapsed();
                    *self.profile.as_mut().unwrap().entry(e.name()).or_default() += d;
                } else {
                    self.tick_exec(e);
                }
            }
        }
        // Executions added while ticking were pushed after the earlier
        // pending ones, so taking the list now keeps the engine's order.
        let pending = std::mem::take(&mut self.uninit_execs);
        let mut inited = Vec::new();
        let mut uninited = Vec::new();
        for mut e in pending {
            if !self.in_spawn_phase() || e.active_during_spawn_phase() {
                self.init_exec(&mut e);
                inited.push(e);
            } else {
                uninited.push(e);
            }
        }
        assert!(self.uninit_execs.is_empty(), "an init added executions; the engine would drop them");
        let in_spawn = self.in_spawn_phase();
        execs.retain(|e| if in_spawn { !e.active_during_spawn_phase() || e.is_active(self) } else { e.is_active(self) });
        execs.extend(inited);
        self.execs = execs;
        self.uninit_execs = uninited;
        self.ticks += 1;
    }

    fn init_exec(&mut self, e: &mut Exec) {
        match e {
            Exec::SpawnTimer | Exec::WinCheck { .. } | Exec::Doomsday(_) | Exec::RecomputeRailCluster | Exec::NoOp | Exec::Tribe(_) => {}
            Exec::Spawn(s) => s.queued_during_spawn_phase = self.in_spawn_phase(),
            Exec::MarkDisconnected { player, disconnected } => {
                // The disconnect snapshot only matters when a player comes back.
                self.players[*player].is_disconnected = *disconnected;
            }
            Exec::Player(p) => {
                p.last_calc = self.ticks as i64 + (simple_hash(&self.players[p.player].info.id) as i64) % TICKS_PER_CLUSTER_CALC;
            }
            Exec::Attack(a) => self.init_attack(a),
            Exec::TransportShip(t) => self.init_transport(t),
            Exec::AllianceRequest(a) => self.init_alliance_request(a),
            Exec::Construction(c) => self.init_construction(c),
            Exec::Port(p) => {
                p.random = Some(PseudoRandom::new(self.ticks as f64));
                p.check_offset = self.ticks % 10;
            }
            Exec::TradeShip(_) | Exec::DefensePost { .. } => {}
            Exec::City { .. } | Exec::Factory { .. } => {}
            Exec::TrainStation(t) => {
                if t.spawn_trains {
                    t.random = Some(PseudoRandom::new(self.ticks as f64));
                }
            }
            Exec::Train(t) => self.init_train(t),
            Exec::Nation(n) => self.init_nation(n),
            Exec::AllianceExtension { from, to_id } => {
                let from = *from;
                if let Some(to) = self.player_index(to_id) {
                    if self.players[from].is_alive() && self.players[to].is_alive() {
                        if let Some(al) = self.alliance_with(from, to) {
                            let a = &mut self.alliances[al];
                            if a.requestor == from {
                                a.ext_requestor = true;
                            } else if a.recipient == from {
                                a.ext_recipient = true;
                            }
                            if a.ext_requestor && a.ext_recipient {
                                a.ext_requestor = false;
                                a.ext_recipient = false;
                                a.expires_at = self.ticks + self.config.alliance_duration;
                            }
                        }
                    }
                }
            }
            Exec::Sam(_) | Exec::Silo { .. } => {}
            Exec::Nuke(n) => {
                if n.speed == -1.0 {
                    n.speed = 10.0;
                }
            }
            Exec::Mirv(m) => self.init_mirv(m),
            Exec::SamMissile(m) => {
                m.seed = self.ticks;
                self.tick_sam_missile(m);
            }
            Exec::UpgradeStructure { player, unit_id, amount } => self.upgrade_structure(*player, *unit_id, *amount),
            Exec::EmbargoAll { player, start } => self.embargo_all(*player, *start),
            Exec::Donate(d) => self.init_donation(d),
            Exec::Warship(w) => self.init_warship(w),
            Exec::Shell(s) => {
                s.random = Some(PseudoRandom::new(self.ticks as f64));
                s.air_seed = self.ticks;
            }
            Exec::MoveWarship { owner, ids, position } => {
                let ids = ids.clone();
                self.move_warships(*owner, &ids, *position);
            }
            Exec::Emoji(e) => self.init_emoji(e),
            Exec::BoatRetreat { .. } => {}
            Exec::TargetPlayer { active, target_id, target, .. } | Exec::Embargo { active, target_id, target, .. } => match self.player_index(target_id) {
                Some(t) => *target = Some(t),
                None => *active = false,
            },
            Exec::Retreat { start_tick, .. } => *start_tick = self.ticks,
            Exec::QuickChat { active, recipient_id } => {
                if self.player_index(recipient_id).is_none() {
                    *active = false;
                }
            }
            Exec::DeleteUnit { active, player, unit_id, unit } => {
                if *active {
                    self.init_delete_unit(*player, *unit_id, unit, active)
                }
            }
            Exec::AllianceReject { requestor_id, recipient } => self.init_alliance_reject(requestor_id, *recipient),
            Exec::BreakAlliance { active, recipient_id, recipient, .. } => match self.player_index(recipient_id) {
                Some(r) => *recipient = Some(r),
                None => *active = false,
            },
        }
    }

    fn tick_exec(&mut self, e: &mut Exec) {
        match e {
            Exec::SpawnTimer => {
                if self.ticks > self.config.num_spawn_phase_turns() {
                    self.end_spawn_phase();
                }
            }
            Exec::Spawn(s) => self.tick_spawn(s),
            Exec::WinCheck { active } => self.tick_win_check(active),
            Exec::Doomsday(d) => self.tick_doomsday(d),
            Exec::RecomputeRailCluster => self.recompute_clusters(),
            Exec::Player(p) => self.tick_player(p),
            Exec::Tribe(t) => self.tick_tribe(t),
            Exec::Attack(a) => self.tick_attack(a),
            Exec::TransportShip(t) => self.tick_transport(t),
            Exec::AllianceRequest(a) => self.tick_alliance_request(a),
            Exec::Construction(c) => self.tick_construction(c),
            Exec::Port(p) => self.tick_port(p),
            // Its targeting is commented out in the engine.
            Exec::DefensePost { unit, active } => {
                if !self.units[*unit].active {
                    *active = false;
                }
            }
            Exec::TradeShip(t) => self.tick_trade_ship(t),
            Exec::City { unit, station_created, active } => self.tick_city(*unit, station_created, active),
            Exec::Factory { unit, station_created, active } => self.tick_factory(*unit, station_created, active),
            Exec::TrainStation(t) => self.tick_train_station(t),
            Exec::Train(t) => self.tick_train(t),
            Exec::Nation(n) => self.tick_nation(n),
            Exec::AllianceExtension { .. } => unreachable!("never active"),
            Exec::Sam(s) => self.tick_sam(s),
            Exec::Nuke(n) => self.tick_nuke(n),
            Exec::Mirv(m) => self.tick_mirv(m),
            Exec::SamMissile(m) => self.tick_sam_missile(m),
            Exec::Silo { unit, active } => self.tick_silo(*unit, active),
            Exec::UpgradeStructure { .. } | Exec::EmbargoAll { .. } => unreachable!("never active"),
            Exec::Donate(d) => self.tick_donation(d),
            Exec::Warship(w) => self.tick_warship(w),
            Exec::Shell(s) => self.tick_shell(s),
            Exec::MoveWarship { .. } => unreachable!("never active"),
            Exec::QuickChat { active, .. } => *active = false,
            Exec::Emoji(e) => self.tick_emoji(e),
            Exec::BoatRetreat { active, player, unit_id } => {
                let boat = self.players[*player].units.iter().copied().find(|&u| self.units[u].id == *unit_id && self.units[u].type_name == TRANSPORT_SHIP);
                if let Some(u) = boat {
                    self.units[u].retreating = true;
                }
                *active = false;
            }
            Exec::Embargo { active, player, start, target, .. } => {
                let (p, t) = (*player, target.expect("initialized"));
                if *start {
                    self.add_embargo(p, t, false);
                } else {
                    self.stop_embargo(p, t);
                }
                *active = false;
            }
            Exec::TargetPlayer { active, requestor, target, .. } => {
                let (r, t) = (*requestor, target.expect("initialized"));
                if self.can_target(r, t) {
                    self.players[r].targets.push((self.ticks, t));
                    self.update_relation(t, r, -40.0);
                }
                *active = false;
            }
            Exec::Retreat { active, player, attack_id, ordered, start_tick } => {
                let attack = self.players[*player].outgoing.iter().copied().find(|&a| self.attacks[a].id == *attack_id);
                if !*ordered {
                    if let Some(a) = attack {
                        self.attacks[a].retreating = true;
                    }
                    *ordered = true;
                }
                if self.ticks >= *start_tick + 20 {
                    if let Some(a) = attack {
                        self.attacks[a].retreated = true;
                    }
                    *active = false;
                }
            }
            Exec::DeleteUnit { active, unit, .. } => self.tick_delete_unit(*unit, active),
            Exec::BreakAlliance { active, requestor, recipient, .. } => {
                self.tick_break_alliance(*requestor, recipient.expect("initialized"));
                *active = false;
            }
            Exec::AllianceReject { .. } => unreachable!("never active"),
            Exec::NoOp | Exec::MarkDisconnected { .. } => unreachable!("never active"),
        }
    }

    /// WinCheckExecution.hasWon: more than the required share of the land
    /// without fallout (lowered by overtime), or the lobby timer or hard limit.
    fn has_won(&self, tiles: usize) -> bool {
        let elapsed = self.ticks_since_start() as f64 / 10.0;
        if self.config.max_timer_minutes.is_some_and(|m| elapsed >= m * 60.0) || elapsed >= 170.0 * 60.0 {
            return true;
        }
        let mut pct = 80.0;
        if self.config.overtime_enabled {
            let past = elapsed.floor() - self.config.overtime_start_minutes * 60.0;
            if past > 0.0 {
                pct = crate::detmath::js_max(0.0, 80.0 - ((past * 2.0) / 60.0).floor());
            }
        }
        let without_fallout = self.map.num_land_tiles as f64 - self.map.num_tiles_with_fallout() as f64;
        tiles as f64 * 100.0 > without_fallout * pct
    }

    /// WinCheckExecution, team games (no ranked modes):
    /// the team with the most tiles wins past 80% of the land without
    /// fallout, unless it's the bots.
    fn tick_win_check(&mut self, active: &mut bool) {
        if self.ticks % 10 != 0 {
            return;
        }
        if !self.config.is_team_mode {
            // checkWinnerFFA (no ranked modes): the largest living player.
            let mut sorted: Vec<usize> = (0..self.players.len()).filter(|&p| self.players[p].is_alive()).collect();
            sorted.sort_by(|&a, &b| self.players[b].tiles.len().cmp(&self.players[a].tiles.len()));
            let Some(&max) = sorted.first() else { return };
            if self.has_won(self.players[max].tiles.len()) {
                self.winner = Some(self.players[max].info.id.clone());
                *active = false;
            }
            return;
        }
        // Map<Team, tiles> in first-seen order over living players.
        let mut teams: Vec<(&str, usize)> = Vec::new();
        for p in self.players.iter().filter(|p| p.is_alive()) {
            let Some(t) = p.team.as_deref() else { continue };
            match teams.iter_mut().find(|(n, _)| *n == t) {
                Some(e) => e.1 += p.tiles.len(),
                None => teams.push((t, p.tiles.len())),
            }
        }
        teams.sort_by(|a, b| b.1.cmp(&a.1)); // stable, like Array.sort
        let Some(&(team, tiles)) = teams.first() else { return };
        if self.has_won(tiles) {
            if team == "Bot" {
                return;
            }
            self.winner = Some(team.to_string());
            *active = false;
        }
    }

    // ── SpawnExecution ──

    pub fn new_spawn(&self, info: PlayerInfo, tile: Option<f64>, from_intent: bool) -> Exec {
        let seed = simple_hash(&info.id) + simple_hash(&self.config.game_id);
        Exec::Spawn(Box::new(SpawnExec {
            random: PseudoRandom::new(seed),
            active: true,
            info,
            tile,
            from_intent,
            queued_during_spawn_phase: false,
        }))
    }

    fn is_valid_ref(&self, tile: f64) -> bool {
        is_integer(tile) && tile >= 0.0 && tile < self.map.num_tiles() as f64
    }

    fn tick_spawn(&mut self, s: &mut SpawnExec) {
        s.active = false;
        if let Some(t) = s.tile {
            if !self.is_valid_ref(t) {
                return;
            }
        }
        let player = match self.players_by_id.get(&s.info.id) {
            Some(&i) => i,
            None => self.add_player(s.info.clone(), None),
        };
        if s.from_intent && !s.queued_during_spawn_phase {
            return;
        }
        if self.config.random_spawn && self.players[player].has_spawned() {
            return;
        }
        let prev_tiles: Vec<u32> = self.players[player].tiles.values().collect();
        for &t in &prev_tiles {
            self.relinquish(t);
        }
        let center = if self.config.random_spawn { None } else { s.tile.map(|t| t as u32) };
        let Some((center, tiles)) = self.get_spawn(s, center) else {
            for &t in &prev_tiles {
                self.conquer(player, t);
            }
            return;
        };
        for &t in &tiles {
            self.conquer(player, t);
        }
        if !self.players[player].has_spawned() {
            self.add_execution(Exec::Player(Box::new(PlayerExec { player, last_calc: 0, active: true })));
            if self.players[player].info.player_type == PlayerType::Bot {
                let e = self.new_tribe(player);
                self.add_execution(e);
            }
        }
        self.players[player].spawn_tile = Some(center);
        if self.config.is_singleplayer && s.info.player_type == PlayerType::Human {
            self.end_spawn_phase();
        }
    }

    const MAX_SPAWN_TRIES: u32 = 1_000;
    const RELAX_MIN_DIST_AT: u32 = 750;

    fn get_spawn(&self, s: &mut SpawnExec, center: Option<u32>) -> Option<(u32, Vec<u32>)> {
        if let Some(center) = center {
            let tiles = self.spawn_tiles(center, false)?;
            return if tiles.is_empty() { None } else { Some((center, tiles)) };
        }
        let area = self.player_index(&s.info.id).and_then(|p| self.team_spawn_area(p));
        let mut tries = 0;
        while tries < Self::MAX_SPAWN_TRIES {
            tries += 1;
            let (cx, cy) = match area {
                Some([x, y, w, h]) => {
                    let cx = s.random.next_int(x as f64, (x + w) as f64);
                    (cx as u32, s.random.next_int(y as f64, (y + h) as f64) as u32)
                }
                None => (s.random.next_int(0.0, self.map.width as f64) as u32, s.random.next_int(0.0, self.map.height as f64) as u32),
            };
            let center = self.map.ref_at(cx, cy);
            if !self.map.is_land(center) || self.map.has_owner(center) || self.map.is_border(center) {
                continue;
            }
            if tries <= Self::RELAX_MIN_DIST_AT {
                let nearby = self.players.iter().any(|p| {
                    p.info.id != s.info.id && p.spawn_tile.is_some_and(|st| self.map.manhattan_dist(st, center) < 30)
                });
                if nearby {
                    continue;
                }
            }
            if let Some(tiles) = self.spawn_tiles(center, true) {
                return Some((center, tiles));
            }
        }
        None
    }

    /// execution/Util getSpawnTiles
    fn spawn_tiles(&self, center: u32, require_all_valid: bool) -> Option<Vec<u32>> {
        let all = self.map.bfs(center, |m, n| eucl_dist_center(m, center, 4.0, n));
        let invalid = |t: u32| self.map.has_owner(t) || !self.map.is_land(t) || self.map.is_impassable(t);
        if !require_all_valid {
            return Some(all.into_iter().filter(|&t| !invalid(t)).collect());
        }
        if all.iter().any(|&t| invalid(t)) {
            None
        } else {
            Some(all)
        }
    }

    // ── PlayerExecution ──

    fn tick_player(&mut self, pe: &mut PlayerExec) {
        let p = pe.player;
        self.decay_relations(p);
        self.check_structures(p);
        if !self.players[p].is_alive() {
            // removeOnDeath: gold, units and alliances go.
            let gold = self.players[p].gold;
            self.remove_gold(p, gold);
            for u in self.players[p].units.clone() {
                if !matches!(self.units[u].type_name, "Atom Bomb" | "Hydrogen Bomb" | "MIRV Warhead" | "MIRV") {
                    self.delete_unit(u);
                }
            }
            self.remove_all_alliances(p);
            pe.active = false;
            return;
        }
        let inc = self.troop_increase_rate(p);
        self.add_troops(p, inc);
        let gold = self.gold_addition_rate(p);
        self.add_gold(p, gold);
        for al in self.players[p].alliances.clone() {
            if self.alliances[al].expires_at <= self.ticks {
                self.expire_alliance(al);
            }
        }
        // Temporary embargoes end after 300 s.
        let ticks = self.ticks;
        self.players[p].embargoes.retain(|e| !(e.is_temporary && ticks as i64 - e.created_at as i64 > 3000));

        let ticks = self.ticks as i64;
        if ticks - pe.last_calc > TICKS_PER_CLUSTER_CALC || self.players[p].tiles.len() < 100 {
            if self.players[p].last_tile_change as i64 >= pe.last_calc {
                pe.last_calc = ticks;
                self.remove_clusters(p);
            }
        }
    }

    fn bump_generation(&mut self) -> u32 {
        self.gen = self.gen.wrapping_add(1);
        if self.gen == 0xffff_ffff {
            self.visited.fill(0);
            self.gen = 1;
        }
        self.gen
    }

    fn bump_generations(&mut self, count: u32) -> u32 {
        let first = self.bump_generation();
        let mut last = first;
        for _ in 1..count {
            last = self.bump_generation();
        }
        if last.wrapping_sub(first) == count - 1 {
            first
        } else {
            self.bump_generations(count)
        }
    }

    /// floodFillWithGen: DFS from the start tiles, in mark order; fills the
    /// bounding box when asked.
    fn flood_fill(
        &mut self,
        gen: u32,
        start: u32,
        diag: bool,
        include: impl Fn(&Game, u32) -> bool,
        bbox: Option<&mut [i64; 4]>,
    ) -> Vec<u32> {
        let mut result = Vec::new();
        let mut stack = std::mem::take(&mut self.dfs_stack);
        stack.clear();
        let track_bbox = bbox.is_some();
        let (mut min_x, mut min_y, mut max_x, mut max_y) = (1e9 as i64, 1e9 as i64, -1e9 as i64, -1e9 as i64);
        let note = |map: &GameMap, t: u32, min_x: &mut i64, min_y: &mut i64, max_x: &mut i64, max_y: &mut i64| {
            let (x, y) = (map.x(t) as i64, map.y(t) as i64);
            *min_x = (*min_x).min(x);
            *min_y = (*min_y).min(y);
            *max_x = (*max_x).max(x);
            *max_y = (*max_y).max(y);
        };
        if self.visited[start as usize] != gen && include(self, start) {
            self.visited[start as usize] = gen;
            result.push(start);
            stack.push(start);
            if track_bbox {
                note(&self.map, start, &mut min_x, &mut min_y, &mut max_x, &mut max_y);
            }
        }
        let mut nb4 = [0u32; 4];
        let mut nb8 = [0u32; 8];
        while let Some(tile) = stack.pop() {
            let ns: &[u32] = if diag {
                let n = self.map.neighbors8(tile, &mut nb8);
                &nb8[..n]
            } else {
                let n = self.map.neighbors4(tile, &mut nb4);
                &nb4[..n]
            };
            for &n in ns {
                if self.visited[n as usize] == gen || !include(self, n) {
                    continue;
                }
                self.visited[n as usize] = gen;
                result.push(n);
                stack.push(n);
                if track_bbox {
                    note(&self.map, n, &mut min_x, &mut min_y, &mut max_x, &mut max_y);
                }
            }
        }
        self.dfs_stack = stack;
        if let Some(b) = bbox {
            *b = [min_x, min_y, max_x, max_y];
        }
        result
    }

    /// PlayerExecution.calculateClusters: the border split into 8-connected
    /// clusters, in border-set order, with their bounding boxes.
    fn calculate_clusters(&mut self, p: usize) -> (Vec<Vec<u32>>, Vec<[i64; 4]>) {
        if self.players[p].border_tiles.len() == 0 {
            return (Vec::new(), Vec::new());
        }
        let border_gen = self.bump_generations(2);
        let current_gen = border_gen + 1;
        for t in self.players[p].border_tiles.values() {
            self.visited[t as usize] = border_gen;
        }
        let mut clusters = Vec::new();
        let mut boxes = Vec::new();
        let include = move |g: &Game, t: u32| g.visited[t as usize] == border_gen;
        let slot_count = self.players[p].border_tiles.slot_count();
        for i in 0..slot_count {
            let Some(start) = self.players[p].border_tiles.slot(i) else {
                continue;
            };
            if self.visited[start as usize] == current_gen {
                continue;
            }
            let mut b = [0i64; 4];
            let cluster = self.flood_fill(current_gen, start, true, &include, Some(&mut b));
            clusters.push(cluster);
            boxes.push(b);
        }
        (clusters, boxes)
    }

    fn remove_clusters(&mut self, p: usize) {
        let _p = crate::prof::scope("remove_clusters");
        let (clusters, boxes) = self.calculate_clusters(p);
        if clusters.is_empty() {
            self.players[p].largest_cluster_box = None;
            return;
        }
        if clusters.len() == 1 {
            self.players[p].largest_cluster_box = Some(boxes[0]);
            let surrounded_by = self.surrounded_by_same_player(p, &clusters[0], boxes[0]);
            let severed = self.severed_land(p, &clusters[0], boxes[0], None, None);
            if let Some(enemy) = surrounded_by {
                if !self.is_friendly(enemy, p, false) {
                    self.remove_cluster(p, &clusters[0]);
                }
            }
            if let Some((main_tiles, candidates)) = severed {
                self.annex_severed_clusters(p, &candidates, &main_tiles);
            }
            return;
        }

        let mut largest = 0;
        for i in 1..clusters.len() {
            if clusters[i].len() > clusters[largest].len() {
                largest = i;
            }
        }
        let t = boxes[largest];
        // Doughnut borders: a cluster enclosed by another cluster of the same
        // territory is a hole, not the main border.
        let mut territory_ids: Option<Vec<i32>> = None;
        let mut next_territory_id = 1;
        let mut is_largest_hole = false;
        for j in 0..clusters.len() {
            if j == largest {
                continue;
            }
            let b = boxes[j];
            if b[0] <= t[0] && b[1] <= t[1] && b[2] >= t[2] && b[3] >= t[3] {
                if territory_ids.is_none() {
                    territory_ids = Some(vec![0; clusters.len()]);
                    for (c, cl) in clusters.iter().enumerate() {
                        for &k in cl {
                            self.cluster_index_map[k as usize] = c as i32 + 1;
                        }
                    }
                }
                let ids = territory_ids.as_mut().unwrap();
                next_territory_id = self.check_and_assign_territory(p, &clusters, largest, j, ids, next_territory_id);
                if ids[largest] == ids[j] {
                    is_largest_hole = true;
                    break;
                }
            }
        }
        if is_largest_hole {
            let mut best_index: i64 = -1;
            let mut best_length: i64 = -1;
            for i in 0..clusters.len() {
                if i == largest || clusters[i].len() as i64 <= best_length {
                    continue;
                }
                let mut is_hole = false;
                let bi = boxes[i];
                for j in 0..clusters.len() {
                    if j == i {
                        continue;
                    }
                    let b = boxes[j];
                    if b[0] <= bi[0] && b[1] <= bi[1] && b[2] >= bi[2] && b[3] >= bi[3] {
                        let ids = territory_ids.as_mut().unwrap();
                        next_territory_id = self.check_and_assign_territory(p, &clusters, i, j, ids, next_territory_id);
                        if ids[i] == ids[j] {
                            is_hole = true;
                            break;
                        }
                    }
                }
                if !is_hole {
                    best_length = clusters[i].len() as i64;
                    best_index = i as i64;
                }
            }
            if best_index != -1 {
                largest = best_index as usize;
            }
        }
        if territory_ids.is_some() {
            for cl in &clusters {
                for &k in cl {
                    self.cluster_index_map[k as usize] = 0;
                }
            }
        }

        let lb = boxes[largest];
        self.players[p].largest_cluster_box = Some(lb);
        let surrounded_by = self.surrounded_by_same_player(p, &clusters[largest], lb);
        let mut classes = vec![0u8; clusters.len()];
        let mut severed_c: Option<Candidates> = None;
        let mut open_c: Option<Candidates> = None;
        for i in 0..clusters.len() {
            if i == largest {
                continue;
            }
            classes[i] = self.classify_cluster(p, &clusters[i], boxes[i]);
            if classes[i] & CLUSTER_SEVERED_CANDIDATE != 0 {
                let s = severed_c.get_or_insert_with(Candidates::default);
                s.clusters.push(clusters[i].clone());
                s.boxes.extend_from_slice(&boxes[i]);
            }
            if classes[i] & CLUSTER_OPEN != 0 {
                let o = open_c.get_or_insert_with(Candidates::default);
                o.clusters.push(clusters[i].clone());
                o.boxes.extend_from_slice(&boxes[i]);
            }
        }
        let severed = self.severed_land(p, &clusters[largest], lb, severed_c.as_ref(), open_c.as_ref());
        if let Some(enemy) = surrounded_by {
            if !self.is_friendly(enemy, p, false) {
                self.remove_cluster(p, &clusters[largest]);
            }
        }
        for i in 0..clusters.len() {
            if classes[i] & CLUSTER_SURROUNDED != 0 {
                self.remove_cluster(p, &clusters[i]);
            }
        }
        if let Some((main_tiles, candidates)) = severed {
            self.annex_severed_clusters(p, &candidates, &main_tiles);
        }
    }

    fn check_and_assign_territory(
        &mut self,
        p: usize,
        clusters: &[Vec<u32>],
        a: usize,
        b: usize,
        ids: &mut [i32],
        mut next_id: i32,
    ) -> i32 {
        if ids[a] != 0 && ids[a] == ids[b] {
            return next_id;
        }
        if ids[a] != 0 && ids[b] != 0 {
            return next_id;
        }
        let start_idx = if ids[b] == 0 { b } else { a };
        let territory_id = next_id;
        next_id += 1;
        let gen = self.bump_generation();
        let start = clusters[start_idx][0];
        self.visited[start as usize] = gen;
        let mut stack = vec![start];
        let me = self.players[p].small_id;
        ids[start_idx] = territory_id;
        let mut nb8 = [0u32; 8];
        while let Some(tile) = stack.pop() {
            let n = self.map.neighbors8(tile, &mut nb8);
            for &nn in &nb8[..n] {
                if self.visited[nn as usize] == gen {
                    continue;
                }
                if self.map.owner_id(nn) == me {
                    self.visited[nn as usize] = gen;
                    stack.push(nn);
                    let c = self.cluster_index_map[nn as usize];
                    if c > 0 {
                        ids[(c - 1) as usize] = territory_id;
                    }
                }
            }
        }
        next_id
    }

    fn surrounded_by_same_player(&self, p: usize, cluster: &[u32], b: [i64; 4]) -> Option<usize> {
        let mut single_enemy: u16 = 0;
        let (mut min_x, mut min_y, mut max_x, mut max_y) = (1e9 as i64, 1e9 as i64, -1e9 as i64, -1e9 as i64);
        let me = self.players[p].small_id;
        let mut nb = [0u32; 4];
        for &tile in cluster {
            if self.map.is_ocean_shore(tile) || self.map.is_on_edge_of_map(tile) {
                return None;
            }
            let n = self.map.neighbors4(tile, &mut nb);
            for &nn in &nb[..n] {
                let owner = self.map.owner_id(nn);
                if owner == 0 {
                    return None;
                }
                if owner != me {
                    if single_enemy == 0 {
                        single_enemy = owner;
                    } else if single_enemy != owner {
                        return None;
                    }
                    let (x, y) = (self.map.x(nn) as i64, self.map.y(nn) as i64);
                    min_x = min_x.min(x);
                    min_y = min_y.min(y);
                    max_x = max_x.max(x);
                    max_y = max_y.max(y);
                }
            }
            if single_enemy == 0 {
                return None;
            }
        }
        if single_enemy == 0 {
            return None;
        }
        let enemy = single_enemy as usize - 1;
        if min_x <= b[0] && min_y <= b[1] && max_x >= b[2] && max_y >= b[3] {
            Some(enemy)
        } else {
            None
        }
    }

    fn classify_cluster(&self, p: usize, cluster: &[u32], b: [i64; 4]) -> u8 {
        let (mut has_enemy, mut has_fallout, mut has_open) = (false, false, false);
        let (mut min_x, mut min_y, mut max_x, mut max_y) = (1e9 as i64, 1e9 as i64, -1e9 as i64, -1e9 as i64);
        let me = self.players[p].small_id;
        let mut nb = [0u32; 4];
        for &tr in cluster {
            if self.map.is_shore(tr) || self.map.is_on_edge_of_map(tr) {
                return CLUSTER_OPEN;
            }
            let n = self.map.neighbors4(tr, &mut nb);
            for &nn in &nb[..n] {
                let owner = self.map.owner_id(nn);
                if owner != 0 && owner != me {
                    has_enemy = true;
                    let (x, y) = (self.map.x(nn) as i64, self.map.y(nn) as i64);
                    min_x = min_x.min(x);
                    min_y = min_y.min(y);
                    max_x = max_x.max(x);
                    max_y = max_y.max(y);
                } else if owner == 0 {
                    if self.map.has_fallout(nn) {
                        has_fallout = true;
                    } else {
                        has_open = true;
                    }
                }
            }
        }
        if !has_enemy {
            return if has_open { CLUSTER_OPEN } else { 0 };
        }
        let mut result = if has_open { CLUSTER_OPEN } else { 0 };
        if min_x <= b[0] && min_y <= b[1] && max_x >= b[2] && max_y >= b[3] {
            result |= CLUSTER_SURROUNDED;
        }
        if has_fallout && !has_open {
            result |= CLUSTER_SEVERED_CANDIDATE;
        }
        result
    }

    fn remove_cluster(&mut self, p: usize, cluster: &[u32]) {
        let me = self.players[p].small_id;
        if cluster.iter().any(|&t| self.map.owner_id(t) != me) {
            return;
        }
        let Some(capturing) = self.capturing_player(p, cluster) else { return };
        let Some(&first) = cluster.first() else { return };
        if !self.is_enclosed(p, first) {
            return;
        }
        let gen = self.bump_generation();
        let include = move |g: &Game, t: u32| g.map.owner_id(t) == me;
        let tiles = self.flood_fill(gen, first, false, &include, None);
        if self.players[p].tiles.len() == tiles.len() {
            self.conquer_player(capturing, p);
        }
        for &t in &tiles {
            self.conquer(capturing, t);
        }
    }

    fn is_enclosed(&mut self, p: usize, start: u32) -> bool {
        let me = self.players[p].small_id;
        let gen = self.bump_generation();
        self.visited[start as usize] = gen;
        let mut stack = vec![start];
        let mut nb = [0u32; 4];
        while let Some(tile) = stack.pop() {
            if self.map.is_on_edge_of_map(tile) {
                return false;
            }
            let n = self.map.neighbors4(tile, &mut nb);
            for &nn in &nb[..n] {
                if self.visited[nn as usize] == gen {
                    continue;
                }
                let owner = self.map.owner_id(nn);
                if owner != 0 && owner != me {
                    continue;
                }
                if owner == 0 && !self.map.is_land(nn) {
                    return false;
                }
                self.visited[nn as usize] = gen;
                stack.push(nn);
            }
        }
        true
    }

    fn capturing_player(&self, p: usize, cluster: &[u32]) -> Option<usize> {
        let me = self.players[p].small_id;
        // Map<Player, count> in insertion order.
        let mut neighbors: Vec<(usize, u32)> = Vec::new();
        let mut nb = [0u32; 4];
        for &t in cluster {
            let n = self.map.neighbors4(t, &mut nb);
            for &nn in &nb[..n] {
                let owner = self.map.owner_id(nn);
                if owner == 0 || owner == me {
                    continue;
                }
                let o = owner as usize - 1;
                if !self.is_friendly(o, p, false) {
                    match neighbors.iter_mut().find(|(q, _)| *q == o) {
                        Some(e) => e.1 += 1,
                        None => neighbors.push((o, 1)),
                    }
                }
            }
        }
        if neighbors.is_empty() {
            return None;
        }
        let mut largest: Option<usize> = None;
        let mut largest_troops = 0.0;
        for &(n, _) in &neighbors {
            for &a in &self.players[n].outgoing {
                let at = &self.attacks[a];
                if at.target == Some(p) && at.troops > largest_troops {
                    largest_troops = at.troops;
                    largest = Some(n);
                }
            }
        }
        if largest.is_some() {
            return largest;
        }
        // getMode
        let mut mode = None;
        let mut max = 0;
        for &(n, c) in &neighbors {
            if c > max {
                max = c;
                mode = Some(n);
            }
        }
        mode
    }

    /// GameImpl.conquerPlayer: gold changes hands, and a teammate takes a disconnected player's ships.
    fn conquer_player(&mut self, conqueror: usize, conquered: usize) {
        if self.players[conquered].is_disconnected && self.is_on_same_team(conqueror, conquered) {
            for u in self.players[conquered].units.clone() {
                if matches!(self.units[u].type_name, WARSHIP | TRANSPORT_SHIP) {
                    self.set_unit_owner(u, conqueror);
                }
            }
        }
        let c = &self.players[conquered];
        let skip_gold = c.attacks_sent == 0 && c.info.player_type == PlayerType::Human;
        let gold = if skip_gold { 0 } else { c.gold };
        let captured = if skip_gold {
            0
        } else if c.info.player_type == PlayerType::Human {
            c.gold / 2
        } else {
            c.gold
        };
        if !skip_gold {
            self.add_gold(conqueror, captured);
            self.remove_gold(conquered, gold);
        }
    }

    // ── AttackExecution ──

    pub fn new_attack(&self, start_troops: Option<f64>, owner: usize, target_id: Option<String>) -> Exec {
        Exec::Attack(Box::new(AttackExec {
            active: true,
            to_conquer: FlatBinaryHeap::default(),
            random: PseudoRandom::new(123.0),
            target_id,
            target: None,
            owner,
            start_troops,
            source_tile: None,
            remove_troops: true,
            attack: None,
            owner_small: 0,
            target_small: 0,
        }))
    }

    fn create_attack(&mut self, owner: usize, target: Option<usize>, troops: f64, source_tile: Option<u32>) -> usize {
        let id = self.players[owner].random.next_id();
        let idx = self.attacks.len();
        self.attacks.push(Attack {
            id,
            target,
            attacker: owner,
            troops,
            source_tile,
            border: TileSet::default(),
            border_size: 0,
            active: true,
            retreating: false,
            retreated: false,
        });
        self.players[owner].outgoing.push(idx);
        if let Some(t) = target {
            self.players[t].incoming.push(idx);
        }
        idx
    }

    fn delete_attack(&mut self, a: usize) {
        let (target, attacker) = (self.attacks[a].target, self.attacks[a].attacker);
        if let Some(t) = target {
            self.players[t].incoming.retain(|&x| x != a);
        }
        self.players[attacker].outgoing.retain(|&x| x != a);
        self.attacks[a].active = false;
    }

    fn set_attack_troops(&mut self, a: usize, troops: f64) {
        self.attacks[a].troops = crate::detmath::js_max(0.0, troops);
    }

    /// PlayerImpl.incomingAttacks: only attackers still alive.
    fn incoming_attacks(&self, p: usize) -> Vec<usize> {
        self.players[p].incoming.iter().copied().filter(|&a| self.players[self.attacks[a].attacker].is_alive()).collect()
    }

    fn init_attack(&mut self, ae: &mut AttackExec) {
        if !ae.active {
            return;
        }
        let owner = ae.owner;
        let target = match &ae.target_id {
            None => None,
            Some(id) => match self.player_index(id) {
                Some(t) => Some(t),
                None => {
                    ae.active = false; // target not found
                    return;
                }
            },
        };
        ae.target = target;
        ae.owner_small = self.players[owner].small_id;
        ae.target_small = self.small(target);
        if target == Some(owner) {
            ae.active = false;
            return;
        }
        if let Some(t) = target {
            if self.is_friendly(owner, t, false) {
                ae.active = false;
                return;
            }
            if self.players[t].info.player_type != PlayerType::Bot && self.players[owner].info.player_type != PlayerType::Bot {
                self.add_embargo(t, owner, true);
                self.reject_incoming_alliance_requests(owner, t);
            }
            if !self.can_attack_player(owner, t) {
                ae.active = false;
                return;
            }
        }
        let mut start_troops = match ae.start_troops {
            Some(s) => s,
            None => {
                let troops = self.players[owner].troops_f();
                if self.players[owner].info.player_type == PlayerType::Bot {
                    troops / 20.0
                } else {
                    troops / 5.0
                }
            }
        };
        if ae.remove_troops {
            start_troops = js_min(self.players[owner].troops_f(), start_troops);
            start_troops = self.remove_troops(owner, start_troops);
        }
        ae.start_troops = Some(start_troops);
        let attack = self.create_attack(owner, target, start_troops, ae.source_tile);
        ae.attack = Some(attack);
        if let Some(src) = ae.source_tile {
            self.add_neighbors(ae, src);
        } else {
            self.refresh_to_conquer(ae);
        }
        // Stats: troops sent (humans only).
        if self.players[owner].info.player_type == PlayerType::Human {
            self.players[owner].attacks_sent += start_troops.floor() as i64;
        }

        for incoming in self.incoming_attacks(owner) {
            if self.attacks[incoming].attacker == target.unwrap_or(usize::MAX) {
                if self.attacks[incoming].troops > self.attacks[attack].troops {
                    let t = self.attacks[incoming].troops - self.attacks[attack].troops;
                    self.set_attack_troops(incoming, t);
                    self.delete_attack(attack);
                    ae.active = false;
                    return;
                } else {
                    let t = self.attacks[attack].troops - self.attacks[incoming].troops;
                    self.set_attack_troops(attack, t);
                    self.delete_attack(incoming);
                }
            }
        }
        for outgoing in self.players[owner].outgoing.clone() {
            if outgoing != attack
                && self.attacks[outgoing].target == self.attacks[attack].target
                && self.attacks[attack].source_tile.is_none()
            {
                let t = self.attacks[attack].troops + self.attacks[outgoing].troops;
                self.set_attack_troops(attack, t);
                self.delete_attack(outgoing);
            }
        }
        if ae.to_conquer.size() == 0 {
            self.retreat(ae, 0.0);
            return;
        }
        let Some(t) = target else { return };
        // Hitting back at whoever just attacked us doesn't anger them, except on Easy.
        if self.config.difficulty != "Easy" && self.is_hostile(owner, t) {
            return;
        }
        let change = self.by_difficulty([-60.0, -70.0, -80.0, -100.0]);
        self.update_relation(t, owner, change);
    }

    fn refresh_to_conquer(&mut self, ae: &mut AttackExec) {
        let a = ae.attack.expect("attack");
        ae.to_conquer.clear();
        self.attacks[a].border_size = 0;
        self.attacks[a].border.clear();
        let slot_count = self.players[ae.owner].border_tiles.slot_count();
        for i in 0..slot_count {
            if let Some(t) = self.players[ae.owner].border_tiles.slot(i) {
                self.add_neighbors(ae, t);
            }
        }
    }

    fn add_neighbors(&mut self, ae: &mut AttackExec, tile: u32) {
        let a = ae.attack.expect("attack");
        let tick_now = self.ticks as f64;
        let mut nb = [0u32; 4];
        let mut nb2 = [0u32; 4];
        let n = self.map.neighbors4(tile, &mut nb);
        for &neighbor in &nb[..n] {
            if self.map.is_water(neighbor) || self.map.is_impassable(neighbor) || self.map.owner_id(neighbor) != ae.target_small {
                continue;
            }
            let at = &mut self.attacks[a];
            let inserted = if ae.source_tile.is_some() {
                at.border.insert(neighbor)
            } else {
                at.border.insert_unordered(neighbor)
            };
            if inserted {
                at.border_size += 1;
            }
            let mut owned_by_me = 0;
            let ni = self.map.neighbors4(neighbor, &mut nb2);
            for &t in &nb2[..ni] {
                if self.map.owner_id(t) == ae.owner_small {
                    owned_by_me += 1;
                }
            }
            let mag = match self.map.terrain_type(neighbor) {
                Terrain::Plains => 1.0,
                Terrain::Highland => 1.5,
                Terrain::Mountain => 2.0,
                _ => 0.0,
            };
            let priority =
                (ae.random.next_int(0.0, 7.0) + 10.0) * (1.0 - owned_by_me as f64 * 0.5 + mag / 2.0) + tick_now;
            ae.to_conquer.enqueue(neighbor, priority);
        }
    }

    fn retreat(&mut self, ae: &mut AttackExec, malus_percent: f64) {
        let a = ae.attack.expect("attack");
        let deaths = self.attacks[a].troops * (malus_percent / 100.0);
        if !ae.remove_troops && ae.source_tile.is_none() {
            let t = self.attacks[a].troops - ae.start_troops.unwrap_or(0.0);
            self.set_attack_troops(a, t);
        }
        let survivors = self.attacks[a].troops - deaths;
        self.add_troops(ae.owner, survivors);
        self.delete_attack(a);
        ae.active = false;
        if self.attacks[a].retreated && self.players[ae.owner].info.player_type == PlayerType::Human {
            self.players[ae.owner].attacks_sent += (-survivors).floor() as i64;
        }
    }

    fn tick_attack(&mut self, ae: &mut AttackExec) {
        let a = ae.attack.expect("Attack not initialized");
        let mut troop_count = self.attacks[a].troops;
        let target = ae.target;
        if !self.attacks[a].active {
            ae.active = false;
            return;
        }
        if self.attacks[a].retreated {
            self.retreat(ae, if target.is_some() { 25.0 } else { 0.0 });
            ae.active = false;
            return;
        }
        if self.attacks[a].retreating {
            return;
        }
        if let Some(t) = target {
            if self.is_friendly(ae.owner, t, false) {
                self.retreat(ae, 0.0);
                return;
            }
        }
        let border_size = self.attacks[a].border_size as f64 + ae.random.next_int(0.0, 5.0);
        let mut tick_budget = 1.0;
        let mut nb = [0u32; 4];
        while tick_budget > 0.0 {
            if troop_count < 1.0 {
                self.delete_attack(a);
                ae.active = false;
                return;
            }
            if ae.to_conquer.size() == 0 {
                self.retreat(ae, 0.0);
                return;
            }
            let tile = ae.to_conquer.dequeue();
            let removed = if ae.source_tile.is_some() {
                self.attacks[a].border.delete(tile)
            } else {
                self.attacks[a].border.delete_unordered(tile)
            };
            if removed {
                self.attacks[a].border_size -= 1;
            }
            if self.map.owner_id(tile) != ae.target_small {
                continue;
            }
            let n = self.map.neighbors4(tile, &mut nb);
            let on_border = nb[..n].iter().any(|&t| self.map.owner_id(t) == ae.owner_small);
            if !on_border {
                continue;
            }
            if !self.map.is_land(tile) || self.map.is_impassable(tile) {
                continue;
            }
            self.add_neighbors(ae, tile);
            let (attacker_loss, defender_loss, tick_fraction) = self.attack_logic(ae.owner, target, troop_count, tile, border_size);
            tick_budget -= tick_fraction;
            troop_count -= attacker_loss;
            self.set_attack_troops(a, troop_count);
            if let Some(t) = target {
                self.remove_troops(t, defender_loss);
            }
            self.conquer(ae.owner, tile);
            self.handle_dead_defender(ae);
        }
    }

    /// EngineConfig.attackLogic (no defense posts or fallout yet).
    fn attack_logic(&self, attacker: usize, defender: Option<usize>, attack_troops: f64, tile: u32, border_size: f64) -> (f64, f64, f64) {
        let defense_post = defender.is_some_and(|d| self.has_unit_nearby(tile, 30, DEFENSE_POST, Some(d)));
        let (mut mag, mut tile_cost) = match self.map.terrain_type(tile) {
            Terrain::Plains => (80.0, 16.5),
            Terrain::Highland => (100.0, 20.0),
            Terrain::Mountain => (120.0, 25.0),
            t => panic!("terrain {t:?} cannot be attacked"),
        };
        if defense_post {
            mag *= 5.0;
            tile_cost *= 3.0;
        }
        if self.map.has_fallout(tile) {
            let ratio = self.map.num_tiles_with_fallout() as f64 / self.map.num_land_tiles as f64;
            let fallout = 5.0 - ratio * 2.0;
            mag *= fallout;
            tile_cost *= fallout;
        }
        let a = &self.players[attacker];
        let Some(d) = defender else {
            let tick_budget = border_size * 2.0;
            let loss = mag / if a.info.player_type == PlayerType::Bot { 10.0 } else { 5.0 };
            return (loss, 0.0, within((2000.0 * tile_cost) / attack_troops, 5.0, 100.0) / tick_budget);
        };
        let dp = &self.players[d];
        let is_disconnected_teammate = dp.is_disconnected && self.is_on_same_team(attacker, d);
        if is_disconnected_teammate {
            mag = 0.0;
        }
        if matches!(a.info.player_type, PlayerType::Human | PlayerType::Nation) && dp.info.player_type == PlayerType::Bot {
            mag *= 0.7;
        }
        let attacker_sigmoid = self.territory_sigmoid_of(a.tiles.len());
        let large_attacker_bonus = 1.0 - 0.7 * attacker_sigmoid;
        let large_defender_bonus = 1.0 - 0.3 * self.territory_sigmoid_of(dp.tiles.len());
        let traitor = self.is_traitor(d);
        let traitor_loss_mod = if traitor { 0.5 } else { 1.0 };
        let traitor_cost_mod = if traitor { 0.8 } else { 1.0 };
        let defender_troops = dp.troops_f();
        let defender_loss = defender_troops / dp.tiles.len() as f64;
        let troop_ratio = defender_troops / attack_troops;
        let attacker_loss = mag
            * traitor_loss_mod
            * within(troop_ratio, 0.6, 2.0)
            * (0.463 * large_attacker_bonus * large_defender_bonus + 0.0039 * defender_loss);
        let speed_cost = (within(troop_ratio, 0.82, 7.5) * within(troop_ratio / 20.0, 1.0, 50.0)) / 8.55;
        let large_attacker_speed_bonus = 1.0 - 0.73 * attacker_sigmoid;
        let tick_fraction =
            (speed_cost * tile_cost * large_attacker_speed_bonus * large_defender_bonus * traitor_cost_mod) / border_size;
        (attacker_loss, defender_loss, tick_fraction)
    }

    fn handle_dead_defender(&mut self, ae: &mut AttackExec) {
        let Some(target) = ae.target else { return };
        if self.players[target].tiles.len() >= 100 {
            return;
        }
        let owner = ae.owner;
        self.conquer_player(owner, target);
        for _pass in 0..100 {
            let mut progressed = false;
            self.players[target].tiles.pause_compaction();
            let mut i = 0;
            while i < self.players[target].tiles.slot_count() {
                let Some(tile) = self.players[target].tiles.slot(i) else {
                    i += 1;
                    continue;
                };
                i += 1;
                let mut nb = [0u32; 4];
                let n = self.map.neighbors4(tile, &mut nb);
                let owner_small = self.players[owner].small_id;
                let borders = nb[..n].iter().any(|&t| self.map.owner_id(t) == owner_small);
                if borders {
                    self.conquer(owner, tile);
                    progressed = true;
                } else {
                    for &neighbor in &nb[..n] {
                        let Some(no) = self.owner_index(neighbor) else { continue };
                        if no != target && !self.is_friendly(no, target, false) {
                            self.conquer(no, tile);
                            progressed = true;
                            break;
                        }
                    }
                }
            }
            self.players[target].tiles.resume_compaction_indexed(&mut self.tile_slot);
            if !progressed {
                break;
            }
        }
    }

    // ── TribeExecution + TribeAttackBehavior / AiAttackBehavior ──

    fn new_tribe(&self, player: usize) -> Exec {
        let mut random = PseudoRandom::new(simple_hash(&self.players[player].info.id));
        let attack_rate = random.next_int(40.0, 80.0);
        let attack_tick = random.next_int(0.0, attack_rate);
        let trigger_ratio = random.next_int(50.0, 60.0) / 100.0;
        let reserve_ratio = random.next_int(30.0, 40.0) / 100.0;
        let expand_ratio = random.next_int(10.0, 20.0) / 100.0;
        Exec::Tribe(Box::new(TribeExec {
            player,
            active: true,
            random,
            neighbors_terra_nullius: true,
            has_behavior: false,
            attack_rate,
            attack_tick,
            trigger_ratio,
            reserve_ratio,
            expand_ratio,
        }))
    }

    fn tick_tribe(&mut self, te: &mut TribeExec) {
        if self.ticks as f64 % te.attack_rate != te.attack_tick {
            return;
        }
        if !self.players[te.player].is_alive() {
            te.active = false;
            return;
        }
        if !te.has_behavior {
            te.has_behavior = true;
            self.send_attack(te, None, false);
            return;
        }
        self.tribe_accept_alliance_requests(te.player);
        self.tribe_delete_next_structure(te.player);
        self.tribe_maybe_attack(te);
    }

    fn tribe_maybe_attack(&mut self, te: &mut TribeExec) {
        if let Some(target) = self.neighbor_traitor_to_attack(te, true) {
            let odds = if self.is_friendly(te.player, target, false) { 6.0 } else { 3.0 };
            if te.random.next_int(0.0, odds) == 0.0 {
                if let Some(al) = self.alliance_with(te.player, target) {
                    self.break_alliance(te.player, al);
                }
                if self.send_attack(te, Some(target), false) {
                    return;
                }
            }
        }
        if te.neighbors_terra_nullius {
            if self.nearby(te.player).iter().any(|n| n.is_none()) {
                if self.send_attack(te, None, false) {
                    return;
                }
            } else {
                te.neighbors_terra_nullius = false;
            }
        }
        self.attack_random_target(te);
    }

    fn neighbor_traitor_to_attack(&self, te: &mut TribeExec, include_allies: bool) -> Option<usize> {
        if self.config.disable_alliances {
            return None;
        }
        let traitors: Vec<usize> = self
            .nearby(te.player)
            .into_iter()
            .flatten()
            .filter(|&n| (include_allies || !self.is_friendly(te.player, n, false)) && self.is_traitor(n))
            .collect();
        if traitors.is_empty() {
            None
        } else {
            Some(traitors[te.random.next_int(0.0, traitors.len() as f64) as usize])
        }
    }

    fn attack_random_target(&mut self, te: &mut TribeExec) {
        if !self.has_trigger_ratio_troops(te) {
            return;
        }
        if let Some(attacker) = self.find_incoming_attack_player(te.player) {
            if self.send_attack(te, Some(attacker), true) {
                return;
            }
        }
        if let Some(target) = self.neighbor_traitor_to_attack(te, false) {
            if te.random.next_int(0.0, 3.0) == 0.0 && self.send_attack(te, Some(target), false) {
                return;
            }
        }
        let neighbors = self.nearby(te.player);
        let shuffled = te.random.shuffle(&neighbors);
        for n in shuffled {
            let Some(n) = n else { continue };
            if self.is_friendly(te.player, n, false) {
                continue;
            }
            if matches!(self.players[n].info.player_type, PlayerType::Nation | PlayerType::Human)
                && te.random.next_int(0.0, 2.0) == 0.0
            {
                continue;
            }
            if self.send_attack(te, Some(n), false) {
                return;
            }
        }
    }

    fn find_incoming_attack_player(&self, p: usize) -> Option<usize> {
        let mut largest = 0.0;
        let mut attacker = None;
        for a in self.incoming_attacks(p) {
            let at = &self.attacks[a];
            if self.is_friendly(p, at.attacker, false) {
                continue;
            }
            if at.troops <= largest {
                continue;
            }
            largest = at.troops;
            attacker = Some(at.attacker);
        }
        attacker
    }

    fn has_trigger_ratio_troops(&self, te: &TribeExec) -> bool {
        self.players[te.player].troops_f() / self.max_troops(te.player) >= te.trigger_ratio
    }

    fn send_attack(&mut self, te: &mut TribeExec, target: Neighbor, _force: bool) -> bool {
        match target {
            Some(t) => {
                if self.shares_border_with(te.player, self.players[t].small_id) {
                    self.send_land_attack(te, Some(t))
                } else {
                    self.send_boat_attack(te, t)
                }
            }
            None => {
                if self.has_land_border_with_terra_nullius(te.player) {
                    self.send_land_attack(te, None)
                } else {
                    self.send_boat_attack_to_nearby_terra_nullius(te)
                }
            }
        }
    }

    fn reserve_troops(&self, te: &TribeExec, target: Neighbor) -> f64 {
        let expansion = match target {
            None => true,
            Some(t) => self.players[t].info.player_type == PlayerType::Bot,
        };
        // High starting gold (3M+) would use maxTroopsWithoutCities here.
        let _ = expansion;
        let max_troops = self.max_troops(te.player);
        let bot_with_structures = target.is_some_and(|t| {
            self.players[t].info.player_type == PlayerType::Bot
                && self.players[t].units.iter().any(|&u| is_structure(self.units[u].type_name))
        });
        let use_reserve = target.is_some() && !bot_with_structures;
        let ratio = if use_reserve { te.reserve_ratio } else { te.expand_ratio };
        max_troops * ratio
    }

    fn send_land_attack(&mut self, te: &mut TribeExec, target: Neighbor) -> bool {
        let troops = self.players[te.player].troops_f() - self.reserve_troops(te, target);
        if troops < 1.0 {
            return false;
        }
        let target_id = target.map(|t| self.players[t].info.id.clone());
        let e = self.new_attack(Some(troops), te.player, target_id);
        self.add_execution(e);
        true
    }

    #[inline]
    fn territory_sigmoid_of(&self, num_tiles: usize) -> f64 {
        self.territory_sigmoid_lut
            .get(num_tiles)
            .copied()
            .unwrap_or_else(|| territory_sigmoid(num_tiles as f64))
    }

    fn has_land_border_with_terra_nullius(&self, p: usize) -> bool {
        let mut nb = [0u32; 4];
        for border in self.players[p].border_tiles.values() {
            let n = self.map.neighbors4(border, &mut nb);
            for &t in &nb[..n] {
                if self.map.is_land(t) && !self.map.is_impassable(t) && !self.map.has_owner(t) {
                    return true;
                }
            }
        }
        false
    }
}

/// largeTerritoryBonus from log(numTiles): the sigmoid is the same for
/// every depth, so callers compute it once (bit-identical to calling it
/// per depth, as the engine does).
fn territory_sigmoid(num_tiles: f64) -> f64 {
    thread_local! {
        static LOG_MIDPOINT: f64 = log(300_000.0);
    }
    LOG_MIDPOINT.with(|mid| sigmoid(log(num_tiles), 2.5, *mid))
}

/// Util.toInt (as an i64; the engine keeps a bigint).
fn to_int(x: f64) -> i64 {
    if x == f64::INFINITY {
        return 9007199254740991;
    }
    if x == f64::NEG_INFINITY {
        return -9007199254740991;
    }
    x.floor() as i64
}

/// Math.sign
fn js_sign(x: f64) -> f64 {
    if x.is_nan() {
        f64::NAN
    } else if x > 0.0 {
        1.0
    } else if x < 0.0 {
        -1.0
    } else {
        x
    }
}

/// How a team game splits its players: the number of teams, and whether it
/// was given as Duos, Trios or Quads (which changes how friends are placed).
#[derive(Clone, Copy)]
pub struct TeamSetup {
    pub count: usize,
    pub duos_trios_quads: bool,
    /// Humans on one team, nations on the other.
    pub humans_vs_nations: bool,
}

/// TeamAssignment.resolveTeamsList for a team count.
pub fn resolve_teams_list(num_teams: usize) -> Vec<String> {
    assert!(num_teams >= 2, "too few teams: {num_teams}");
    if num_teams < 8 {
        let all = ["Red", "Blue", "Yellow", "Green", "Purple", "Orange", "Teal"];
        return all[..num_teams].iter().map(|s| s.to_string()).collect();
    }
    (1..=num_teams).map(|i| format!("Team {i}")).collect()
}

fn set_team(order: &mut Vec<usize>, result: &mut HashMap<usize, Option<String>>, i: usize, t: Option<String>) {
    if result.insert(i, t).is_none() {
        order.push(i);
    }
}

/// TeamAssignment.assignTeams. Returns players in the order the engine adds
/// them (the result map's insertion order); None means kicked.
pub fn assign_teams(
    players: &[PlayerInfo],
    teams: &[String],
    is_duos_trios_quads: bool,
    max_team_size: usize,
) -> Vec<(PlayerInfo, Option<String>)> {
    let mut order: Vec<usize> = Vec::new();
    let mut result: HashMap<usize, Option<String>> = HashMap::new();
    let mut team_count: HashMap<String, usize> = HashMap::new();
    let mut unpinned = Vec::new();
    for (i, p) in players.iter().enumerate() {
        match p.team_index.and_then(|ti| teams.get(ti)) {
            Some(t) => {
                set_team(&mut order, &mut result, i, Some(t.clone()));
                *team_count.entry(t.clone()).or_default() += 1;
            }
            None => unpinned.push(i),
        }
    }
    let mut clan_order: Vec<String> = Vec::new();
    let mut clans: HashMap<String, Vec<usize>> = HashMap::new();
    let mut non_clan = Vec::new();
    for &i in &unpinned {
        match players[i].clan_tag.as_deref().filter(|c| !c.is_empty()) {
            Some(tag) => {
                if !clans.contains_key(tag) {
                    clan_order.push(tag.to_string());
                }
                clans.entry(tag.to_string()).or_default().push(i);
            }
            None => non_clan.push(i),
        }
    }
    let mut sorted: Vec<Vec<usize>> = clan_order.iter().map(|t| clans[t].clone()).collect();
    sorted.sort_by(|a, b| b.len().cmp(&a.len()));
    for clan in &sorted {
        let mut team: Option<&String> = None;
        let mut team_size = 0;
        for t in teams {
            let p = team_count.get(t).copied().unwrap_or(0);
            if team.is_some() && team_size <= p {
                continue;
            }
            team_size = p;
            team = Some(t);
        }
        let Some(team) = team else { continue };
        for &i in clan {
            if team_size < max_team_size {
                team_size += 1;
                set_team(&mut order, &mut result, i, Some(team.clone()));
            } else {
                set_team(&mut order, &mut result, i, None);
            }
        }
        team_count.insert(team.clone(), team_size);
    }
    let present: HashSet<&str> = players.iter().filter_map(|p| p.client_id.as_deref()).collect();
    let mut friend_graph: HashMap<String, Vec<String>> = HashMap::new();
    let mut add_edge = |a: &str, b: &str| {
        let s = friend_graph.entry(a.to_string()).or_default();
        if !s.iter().any(|x| x == b) {
            s.push(b.to_string());
        }
    };
    for p in players {
        let Some(cid) = p.client_id.as_deref() else { continue };
        for f in &p.friends {
            if !present.contains(f.as_str()) {
                continue;
            }
            add_edge(cid, f);
            add_edge(f, cid);
        }
    }
    let mut team_by_client: HashMap<String, String> = HashMap::new();
    for &i in &order {
        if let (Some(cid), Some(Some(t))) = (players[i].client_id.as_ref(), result.get(&i)) {
            team_by_client.insert(cid.clone(), t.clone());
        }
    }
    let nation_players: Vec<usize> = non_clan.iter().copied().filter(|&i| players[i].player_type == PlayerType::Nation).collect();
    assert!(nation_players.is_empty(), "nation team shuffling is not ported yet");
    let others: Vec<usize> = non_clan.iter().copied().filter(|&i| players[i].player_type != PlayerType::Nation).collect();
    for i in others {
        let p = &players[i];
        let my_friends = p.client_id.as_ref().and_then(|c| friend_graph.get(c));
        let mut best: Option<&String> = None;
        let mut best_friends: i64 = -1;
        let mut best_size: f64 = if is_duos_trios_quads { -1.0 } else { f64::INFINITY };
        for t in teams {
            let size = team_count.get(t).copied().unwrap_or(0);
            if size >= max_team_size {
                continue;
            }
            let mut on_team = 0;
            if let Some(fs) = my_friends {
                for f in fs {
                    if team_by_client.get(f) == Some(t) {
                        on_team += 1;
                    }
                }
            }
            let size_f = size as f64;
            if on_team > best_friends
                || (on_team == best_friends && if is_duos_trios_quads { size_f > best_size } else { size_f < best_size })
            {
                best_friends = on_team;
                best_size = size_f;
                best = Some(t);
            }
        }
        match best {
            None => set_team(&mut order, &mut result, i, None),
            Some(t) => {
                *team_count.entry(t.clone()).or_default() += 1;
                set_team(&mut order, &mut result, i, Some(t.clone()));
                if let Some(cid) = &p.client_id {
                    team_by_client.insert(cid.clone(), t.clone());
                }
            }
        }
    }
    order.into_iter().map(|i| (players[i].clone(), result[&i].clone())).collect()
}

