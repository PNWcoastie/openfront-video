use super::*;

// ── Nations (NationExecution) ──

#[derive(Clone)]
pub struct NationInfo {
    pub info: PlayerInfo,
    /// Spawn cell from the map manifest.
    pub spawn_cell: Option<(i64, i64)>,
}

pub(super) struct BotSurroundings {
    pub(super) bot: usize,
    pub(super) borders_us: bool,
    /// Touches water, the map edge, free land or anyone but us and bots we may attack.
    pub(super) open: bool,
    /// The other bots it borders, in discovery order.
    pub(super) bots: Vec<usize>,
}

pub struct NationExec {
    pub(super) active: bool,
    pub(super) random: PseudoRandom,
    pub(super) nation: NationInfo,
    pub(super) player: Option<usize>,
    pub(super) behaviors_initialized: bool,
    pub(super) spawn_exec_added: bool,
    pub(super) attack_rate: f64,
    pub(super) attack_tick: f64,
    pub(super) trigger_ratio: f64,
    pub(super) reserve_ratio: f64,
    pub(super) expand_ratio: f64,
    pub(super) embargo_malus_applied: Vec<usize>,
    pub(super) last_emoji_sent: Vec<(usize, u32)>,
    pub(super) game_over: bool,
    pub(super) bot_attack_troops_sent: f64,
    /// Our landed beachheads already followed up by land, while they last.
    pub(super) followed_landings: Vec<usize>,
    pub(super) boat_routes: Option<Vec<(usize, Option<BoatRoute>)>>,
    pub(super) land_neighbors: Option<Vec<usize>>,
    pub(super) structures: NationStructureState,
    pub(super) tracked_transports: Vec<usize>,
    pub(super) tracked_trade: Vec<usize>,
    pub(super) tracked_incoming: Vec<usize>,
    pub(super) dealt_with: Vec<usize>,
    pub(super) nukes: NationNukeState,
}

impl Game {
    pub fn new_nation(&self, nation: NationInfo) -> Exec {
        let mut random = PseudoRandom::new(simple_hash(&nation.info.id) + simple_hash(&self.config.game_id));
        let trigger_ratio = random.next_int(50.0, 60.0) / 100.0;
        let reserve_ratio = random.next_int(30.0, 40.0) / 100.0;
        let expand_ratio = random.next_int(10.0, 20.0) / 100.0;
        Exec::Nation(Box::new(NationExec {
            active: true,
            random,
            nation,
            player: None,
            behaviors_initialized: false,
            spawn_exec_added: false,
            attack_rate: 0.0,
            attack_tick: 0.0,
            trigger_ratio,
            reserve_ratio,
            expand_ratio,
            embargo_malus_applied: Vec::new(),
            last_emoji_sent: Vec::new(),
            game_over: false,
            bot_attack_troops_sent: 0.0,
            followed_landings: Vec::new(),
            boat_routes: None,
            land_neighbors: None,
            structures: NationStructureState::default(),
            tracked_transports: Vec::new(),
            tracked_trade: Vec::new(),
            tracked_incoming: Vec::new(),
            dealt_with: Vec::new(),
            nukes: NationNukeState::default(),
        }))
    }

    pub(super) fn init_nation(&mut self, ne: &mut NationExec) {
        ne.attack_rate = match self.config.difficulty.as_str() {
            "Easy" => ne.random.next_int(65.0, 100.0),
            "Medium" => ne.random.next_int(55.0, 70.0),
            "Hard" => ne.random.next_int(45.0, 60.0),
            "Impossible" => ne.random.next_int(30.0, 50.0),
            d => panic!("unknown difficulty {d}"),
        };
        ne.attack_tick = ne.random.next_int(0.0, ne.attack_rate);
        ne.player = Some(match self.player_index(&ne.nation.info.id) {
            Some(p) => p,
            None => self.add_player(ne.nation.info.clone(), None),
        });
    }

    pub(super) fn tick_nation(&mut self, ne: &mut NationExec) {
        // ENGINE_RS_TRACE_NATION=<smallID>:<tick>: print that nation's random draws on that tick.
        let trace = std::env::var("ENGINE_RS_TRACE_NATION").ok().is_some_and(|v| {
            ne.player.is_some_and(|p| v == format!("{}:{}", self.players[p].small_id, self.ticks) || v == format!("{}:all", self.players[p].small_id))
        });
        if trace {
            eprintln!("tick {}", self.ticks);
        }
        crate::rng::TRACE.store(trace, std::sync::atomic::Ordering::Relaxed);
        self.tick_nation_inner(ne);
        crate::rng::TRACE.store(false, std::sync::atomic::Ordering::Relaxed);
    }

    pub(super) fn tick_nation_inner(&mut self, ne: &mut NationExec) {
        let ticks = self.ticks as f64;
        let Some(p) = ne.player else { return };
        if ne.behaviors_initialized
            && self.players[p].is_alive()
            && self.config.difficulty != "Easy"
            && self.units_constructed(p, PORT) > 0
        {
            self.nation_track_ships_and_retaliate(ne);
        }
        if self.in_spawn_phase() {
            if self.players[p].has_spawned() {
                if ticks % ne.attack_rate != ne.attack_tick {
                    return;
                }
            } else if ne.spawn_exec_added {
                return;
            }
            let Some((cx, cy)) = ne.nation.spawn_cell else {
                let e = self.new_spawn(ne.nation.info.clone(), None, false);
                self.add_execution(e);
                ne.spawn_exec_added = true;
                return;
            };
            // Outside its team's spawn area, the nation spawns randomly within it instead.
            if let Some([x, y, w, h]) = self.team_spawn_area(p) {
                if !(cx >= x && cx < x + w && cy >= y && cy < y + h) {
                    let e = self.new_spawn(ne.nation.info.clone(), None, false);
                    self.add_execution(e);
                    ne.spawn_exec_added = true;
                    return;
                }
            }
            // randomSpawnLand
            let mut found = None;
            let (w, h) = (self.map.width as f64, self.map.height as f64);
            for _ in 0..50 {
                let x = ne.random.next_int(cx as f64 - 25.0, cx as f64 + 25.0);
                let y = ne.random.next_int(cy as f64 - 25.0, cy as f64 + 25.0);
                if !(x >= 0.0 && x < w && y >= 0.0 && y < h) {
                    continue;
                }
                let tile = self.map.ref_at(x as u32, y as u32);
                if self.map.is_land(tile) && !self.map.has_owner(tile) && !self.map.is_impassable(tile) {
                    if self.map.terrain_type(tile) == Terrain::Mountain && ne.random.next_int(0.0, 2.0) == 0.0 {
                        continue;
                    }
                    found = Some(tile);
                    break;
                }
            }
            let Some(tile) = found else { return };
            let e = self.new_spawn(ne.nation.info.clone(), Some(tile as f64), false);
            self.add_execution(e);
            ne.spawn_exec_added = true;
            return;
        }
        if ne.spawn_exec_added && !self.players[p].has_spawned() {
            return;
        }
        if !self.players[p].is_alive() {
            ne.active = false;
            return;
        }
        if !ne.behaviors_initialized {
            ne.behaviors_initialized = true;
            self.nation_force_send_attack(ne, None);
            return;
        }
        self.nation_follow_up_landings(ne);
        self.nation_follow_up_strikes(ne);
        if ticks % ne.attack_rate != ne.attack_tick {
            if self.players[p].is_alive() {
                let offset = ticks % ne.attack_rate;
                let one_third = (ne.attack_tick + (ne.attack_rate / 3.0).floor()) % ne.attack_rate;
                let two_thirds = (ne.attack_tick + ((ne.attack_rate * 2.0) / 3.0).floor()) % ne.attack_rate;
                if offset == one_third || offset == two_thirds {
                    self.nation_handle_structures(ne);
                }
            }
            return;
        }
        self.nation_maybe_send_casual_emoji(ne);
        self.nation_update_relations_from_embargos(ne);
        self.nation_handle_alliance_requests(ne);
        self.nation_handle_alliance_extension_requests(ne);
        self.nation_consider_mirv(ne);
        self.nation_handle_structures(ne);
        self.nation_maybe_spawn_warship(ne);
        self.nation_handle_embargoes_to_hostile(ne);
        self.nation_maybe_attack(ne);
        self.nation_counter_warship_infestation(ne);
        self.nation_maybe_send_nuke(ne);
    }

}

