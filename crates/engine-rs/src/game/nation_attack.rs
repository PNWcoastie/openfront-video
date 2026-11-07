use super::*;
use super::nation::BotSurroundings;
use super::units::closest_two_tiles;

// ── Nation AI: attacks, alliances, emoji, embargoes (NationAttackBehavior and
// its AiAttackBehavior base, NationAllianceBehavior, NationEmojiBehavior,
// NationUtils). Only Medium difficulty in FFA is ported. ──

/// Schemas.ts emojiTable, flattened.
const EMOJI_TABLE: [&str; 60] = [
    "😀", "😊", "🥰", "😇", "😎", "😞", "🥺", "😭", "😱", "😡", "😈", "🤡", "🥱", "🫡", "🖕", "👋", "👏", "✋", "🙏", "💪",
    "👍", "👎", "🫴", "🤌", "🤦‍♂️", "🤝", "🆘", "🕊️", "🏳️", "⏳", "🔥", "💥", "💀", "☢️", "⚠️", "↖️", "⬆️", "↗️", "👑", "🥇",
    "⬅️", "🎯", "➡️", "🥈", "🥉", "↙️", "⬇️", "↘️", "❤️", "💔", "💰", "⚓", "⛵", "🏡", "🛡️", "🏭", "🚂", "❓", "🐔", "🐀",
];

pub(super) fn emoji_ids(list: &[&str]) -> Vec<usize> {
    list.iter().map(|e| EMOJI_TABLE.iter().position(|t| t == e).expect("emoji")).collect()
}

/// Relation: Hostile 0, Distrustful 1, Neutral 2, Friendly 3.
pub(super) fn relation_from_value(v: f64) -> u8 {
    if v < -50.0 {
        0
    } else if v < 0.0 {
        1
    } else if v < 50.0 {
        2
    } else {
        3
    }
}

#[derive(Clone, Copy)]
pub struct BoatRoute {
    pub(super) landing: u32,
    pub(super) length: f64,
    pub(super) blocker: Option<usize>,
}

pub struct EmojiExec {
    pub(super) active: bool,
    pub(super) from: usize,
    pub(super) recipient_id: String,
    /// None: everyone (AllPlayers).
    pub(super) recipient: Option<usize>,
    pub(super) emoji: usize,
    pub(super) random: Option<PseudoRandom>,
}

impl EmojiExec {
    pub fn new(from: usize, recipient_id: String, emoji: usize) -> EmojiExec {
        EmojiExec { active: true, from, recipient_id, recipient: None, emoji, random: None }
    }
}

impl Game {
    // ── Player helpers ──

    pub(super) fn relation(&self, p: usize, other: usize) -> u8 {
        relation_from_value(self.players[p].relation_value(other))
    }

    /// Players EmbargoAllExecution acts on: alive, not bots, not teammates or allies.
    pub(super) fn embargo_all_targets(&self, me: usize) -> Vec<usize> {
        self.alive_players()
            .into_iter()
            .filter(|&p| {
                p != me
                    && self.players[p].info.player_type != PlayerType::Bot
                    && !self.is_on_same_team(me, p)
                    && !self.is_allied_with(me, p)
            })
            .collect()
    }

    /// EmbargoAllExecution.init (canEmbargoAll: a 100-tick cooldown and someone to act on).
    pub(super) fn embargo_all(&mut self, me: usize, start: bool) {
        let targets = self.embargo_all_targets(me);
        if (self.ticks as i64 - self.players[me].last_embargo_all_tick) < 100 || targets.is_empty() {
            return;
        }
        for p in targets {
            let has = self.has_embargo_against(me, p);
            if start && !has {
                self.add_embargo(me, p, false);
            } else if !start && has {
                self.stop_embargo(me, p);
            }
        }
        self.players[me].last_embargo_all_tick = self.ticks as i64;
    }

    /// PlayerImpl.addGold: also counts toward lifetime income.
    pub(super) fn add_gold(&mut self, p: usize, gold: i64) {
        self.players[p].gold += gold;
        self.players[p].gold_earned += gold;
    }

    /// One of four values by difficulty: Easy, Medium, Hard, Impossible.
    pub(super) fn by_difficulty<T: Copy>(&self, v: [T; 4]) -> T {
        match self.config.difficulty.as_str() {
            "Easy" => v[0],
            "Medium" => v[1],
            "Hard" => v[2],
            "Impossible" => v[3],
            d => panic!("unknown difficulty {d}"),
        }
    }

    /// Nation AI is ported for Easy, Medium, Hard, and Impossible.
    pub(super) fn assert_nation_difficulty(&self) {
        assert!(
            matches!(self.config.difficulty.as_str(), "Easy" | "Medium" | "Hard" | "Impossible"),
            "{} nations are not ported yet",
            self.config.difficulty
        );
    }

    pub(super) fn alive_players(&self) -> Vec<usize> {
        (0..self.players.len()).filter(|&p| self.players[p].is_alive()).collect()
    }

    pub(super) fn allies(&self, p: usize) -> Vec<usize> {
        self.players[p].alliances.iter().map(|&a| self.alliances[a].other(p)).collect()
    }

    pub(super) fn targets(&self, p: usize) -> Vec<usize> {
        let ticks = self.ticks as i64;
        self.players[p].targets.iter().filter(|t| ticks - (t.0 as i64) < 100).map(|t| t.1).collect()
    }

    pub(super) fn can_target(&self, p: usize, other: usize) -> bool {
        if p == other || self.is_friendly(p, other, false) {
            return false;
        }
        let ticks = self.ticks as i64;
        !self.players[p].targets.iter().any(|t| ticks - (t.0 as i64) < 150)
    }

    pub(super) fn has_manual_embargo_against(&self, p: usize, other: usize) -> bool {
        self.players[p].embargoes.iter().any(|e| e.target == other && !e.is_temporary)
    }

    pub(super) fn stop_embargo(&mut self, p: usize, other: usize) {
        self.players[p].embargoes.retain(|e| e.target != other);
    }

    /// PlayerImpl.canSendEmoji
    pub(super) fn can_send_emoji(&self, p: usize, recipient: Option<usize>) -> bool {
        if recipient == Some(p) {
            return false;
        }
        let ticks = self.ticks as i64;
        let recent = self.players[p].outgoing_emojis.iter().filter(|e| e.0 == recipient && ticks - (e.1 as i64) < 50).count();
        let limit = if self.players[p].info.player_type == PlayerType::Human { 5 } else { 1 };
        recent < limit
    }

    pub(super) fn sum_troops(&self, attacks: &[usize]) -> f64 {
        attacks.iter().map(|&a| self.attacks[a].troops).sum()
    }

    pub(super) fn incoming_troops(&self, p: usize) -> f64 {
        self.incoming_attacks(p).iter().fold(0.0, |s, &a| s + self.attacks[a].troops)
    }

    pub(super) fn outgoing_troops(&self, p: usize) -> f64 {
        self.players[p].outgoing.iter().fold(0.0, |s, &a| s + self.attacks[a].troops)
    }

    /// NationUtils.findRunawayLeader (Medium: 3 times the runner-up's land)
    pub(super) fn find_runaway_leader(&self) -> Option<usize> {
        if self.config.is_team_mode {
            return None;
        }
        let factor = match self.config.difficulty.as_str() {
            "Easy" => return None,
            "Medium" => 3.0,
            "Hard" => 2.0,
            _ => 1.5,
        };
        let (mut leader, mut runner_up): (Option<usize>, Option<usize>) = (None, None);
        for p in self.alive_players() {
            if self.players[p].info.player_type == PlayerType::Bot {
                continue;
            }
            let tiles = self.players[p].tiles.len();
            if leader.is_none_or(|l| tiles > self.players[l].tiles.len()) {
                runner_up = leader;
                leader = Some(p);
            } else if runner_up.is_none_or(|r| tiles > self.players[r].tiles.len()) {
                runner_up = Some(p);
            }
        }
        let (l, r) = (leader?, runner_up?);
        if self.players[l].tiles.len() as f64 >= self.players[r].tiles.len() as f64 * factor {
            Some(l)
        } else {
            None
        }
    }

    pub(super) fn bounding_box_center(&self, p: usize) -> (i64, i64) {
        let [min_x, min_y, max_x, max_y] = match self.players[p].largest_cluster_box {
            Some(b) => b,
            None => {
                let mut b = [i64::MAX, i64::MAX, i64::MIN, i64::MIN];
                for t in self.players[p].border_tiles.values() {
                    let (x, y) = (self.map.x(t) as i64, self.map.y(t) as i64);
                    b = [b[0].min(x), b[1].min(y), b[2].max(x), b[3].max(y)];
                }
                b
            }
        };
        (min_x + (max_x - min_x).div_euclid(2), min_y + (max_y - min_y).div_euclid(2))
    }

    // ── NationEmojiBehavior ──

    pub(super) fn emoji_should_send(&self, ne: &mut NationExec, other: Option<usize>, limit_by_time: bool) -> bool {
        let Some(o) = other else { return true };
        let p = ne.player.unwrap();
        if self.players[p].info.player_type == PlayerType::Bot || self.players[o].info.player_type != PlayerType::Human {
            return false;
        }
        if limit_by_time {
            let last = ne.last_emoji_sent.iter().find(|e| e.0 == o).map_or(-300, |e| e.1 as i64);
            if self.ticks as i64 - last <= 300 {
                return false;
            }
            match ne.last_emoji_sent.iter_mut().find(|e| e.0 == o) {
                Some(e) => e.1 = self.ticks,
                None => ne.last_emoji_sent.push((o, self.ticks)),
            }
        }
        true
    }

    pub(super) fn emoji_send(&mut self, ne: &mut NationExec, other: Option<usize>, list: &[&str]) {
        if !self.emoji_should_send(ne, other, false) {
            return;
        }
        let p = ne.player.unwrap();
        if !self.can_send_emoji(p, other) {
            return;
        }
        let ids = emoji_ids(list);
        let emoji = ids[ne.random.next_int(0.0, ids.len() as f64) as usize];
        let recipient_id = match other {
            None => "AllPlayers".to_string(),
            Some(o) => self.players[o].info.id.clone(),
        };
        self.add_execution(Exec::Emoji(Box::new(EmojiExec { active: true, from: p, recipient_id, recipient: None, emoji, random: None })));
    }

    pub(super) fn emoji_maybe_send(&mut self, ne: &mut NationExec, other: usize, list: &[&str]) {
        if !self.emoji_should_send(ne, Some(other), true) {
            return;
        }
        self.emoji_send(ne, Some(other), list);
    }

    pub(super) fn emoji_maybe_send_attack(&mut self, ne: &mut NationExec, other: usize) {
        if !self.emoji_should_send(ne, Some(other), true) {
            return;
        }
        let p = ne.player.unwrap();
        if self.relation(p, other) >= 2 {
            if ne.random.next_int(0.0, 2.0) != 0.0 {
                return;
            }
            self.emoji_send(ne, Some(other), &["😈"]);
            return;
        }
        if ne.random.next_int(0.0, 4.0) != 0.0 {
            return;
        }
        self.emoji_send(ne, Some(other), &["😡"]);
    }

    pub(super) fn nation_maybe_send_casual_emoji(&mut self, ne: &mut NationExec) {
        if ne.game_over {
            return;
        }
        let p = ne.player.unwrap();
        let chance = |ne: &mut NationExec, n: f64| ne.random.next_int(0.0, n) == 0.0;
        // checkOverwhelmedByAttacks
        if chance(ne, 16.0) {
            let incoming = self.incoming_attacks(p);
            if !incoming.is_empty() && self.sum_troops(&incoming) >= self.players[p].troops_f() * 3.0 {
                self.emoji_send(ne, None, &["💀", "🆘", "😱", "🥺", "😭", "😞", "🫡", "👋"]);
            }
        }
        // checkVerySmallAttack
        if chance(ne, 8.0) {
            let incoming = self.incoming_attacks(p);
            let ours = self.players[p].troops_f();
            if !incoming.is_empty() && ours > 0.0 {
                for a in incoming {
                    let attacker = self.attacks[a].attacker;
                    if self.players[attacker].info.player_type != PlayerType::Human {
                        continue;
                    }
                    if self.attacks[a].troops < ours * 0.1 {
                        let list: &[&str] = if chance(ne, 2.0) { &["❓", "🤡"] } else { &["🥱"] };
                        self.emoji_maybe_send(ne, attacker, list);
                    }
                }
            }
        }
        // congratulateWinner
        if let Some(w) = self.winner.clone() {
            ne.game_over = true;
            let winner = self.player_index(&w);
            if winner == Some(p) || self.players[p].team.as_ref() == Some(&w) {
                if chance(ne, 2.0) {
                    self.emoji_send(ne, None, &["👑", "🥇", "😎", "💪", "😀"]);
                }
            } else if self.config.is_team_mode {
                // Every nation congratulates when another team won.
                self.emoji_send(ne, None, &["👏"]);
            } else {
                if let Some(w) = winner {
                    let mut nations: Vec<usize> =
                        self.alive_players().into_iter().filter(|&q| self.players[q].info.player_type == PlayerType::Nation).collect();
                    nations.sort_by(|&a, &b| self.players[b].tiles.len().cmp(&self.players[a].tiles.len()));
                    if nations.first() == Some(&p) {
                        self.emoji_send(ne, Some(w), &["👏"]);
                    }
                }
            }
        }
        // brag
        if chance(ne, 300.0) {
            let mut sorted = self.alive_players();
            sorted.sort_by(|&a, &b| self.players[b].tiles.len().cmp(&self.players[a].tiles.len()));
            if sorted.first() == Some(&p) {
                self.emoji_send(ne, None, &["👑", "🥇", "💪"]);
            }
        }
        // charmAllies
        if chance(ne, 250.0) {
            let humans: Vec<usize> = self.allies(p).into_iter().filter(|&a| self.players[a].info.player_type == PlayerType::Human).collect();
            if !humans.is_empty() {
                let ally = humans[ne.random.next_int(0.0, humans.len() as f64) as usize];
                let list: &[&str] = if chance(ne, 3.0) { &["❤️", "😊", "🥰"] } else { &["🤝", "😇", "💪"] };
                self.emoji_send(ne, Some(ally), list);
            }
        }
        // annoyTraitors
        if chance(ne, 3.0) {
            for traitor in self.alive_players() {
                if self.players[traitor].info.player_type != PlayerType::Human || !self.is_traitor(traitor) {
                    continue;
                }
                let mut mocker: Option<usize> = None;
                for q in self.alive_players() {
                    if self.players[q].info.player_type != PlayerType::Nation || self.is_friendly(q, traitor, false) {
                        continue;
                    }
                    if mocker.is_none_or(|m| self.relation(q, traitor) < self.relation(m, traitor)) {
                        mocker = Some(q);
                    }
                }
                if mocker == Some(p) {
                    self.emoji_send(ne, Some(traitor), &["🤡", "🤦‍♂️"]);
                }
            }
        }
        // findRat
        if self.ticks >= 6000 && chance(ne, 10000.0) {
            let threshold = self.map.num_land_tiles as f64 * 0.01;
            let small: Vec<usize> = self
                .alive_players()
                .into_iter()
                .filter(|&q| {
                    let t = self.players[q].tiles.len();
                    self.players[q].info.player_type == PlayerType::Human && (t as f64) < threshold && t > 0
                })
                .collect();
            if !small.is_empty() {
                let rat = small[ne.random.next_int(0.0, small.len() as f64) as usize];
                self.emoji_send(ne, Some(rat), &["🐀"]);
            }
        }
        // greetNearbyPlayers
        if self.ticks <= 600 && chance(ne, 250.0) {
            let humans: Vec<usize> =
                self.nearby(p).into_iter().flatten().filter(|&q| self.players[q].info.player_type == PlayerType::Human).collect();
            if !humans.is_empty() {
                let n = humans[ne.random.next_int(0.0, humans.len() as f64) as usize];
                self.emoji_send(ne, Some(n), &["👋"]);
            }
        }
    }

    // ── EmojiExecution ──

    pub(super) fn init_emoji(&mut self, ee: &mut EmojiExec) {
        ee.random = Some(PseudoRandom::new(self.ticks as f64));
        if ee.recipient_id == "AllPlayers" {
            ee.recipient = None;
        } else {
            match self.player_index(&ee.recipient_id) {
                Some(r) => ee.recipient = Some(r),
                None => ee.active = false,
            }
        }
    }

    pub(super) fn tick_emoji(&mut self, ee: &mut EmojiExec) {
        ee.active = false;
        let Some(&emoji) = EMOJI_TABLE.get(ee.emoji) else { return };
        if !self.can_send_emoji(ee.from, ee.recipient) {
            return;
        }
        self.players[ee.from].outgoing_emojis.push((ee.recipient, self.ticks));
        // respondToEmoji
        let Some(r) = ee.recipient else { return };
        if self.players[r].info.player_type != PlayerType::Nation || !self.can_send_emoji(r, Some(ee.from)) {
            return;
        }
        let random = ee.random.as_mut().unwrap();
        let reply = |g: &mut Game, list: &[&str], random: &mut PseudoRandom| {
            let ids = emoji_ids(list);
            let e = ids[random.next_int(0.0, ids.len() as f64) as usize];
            let to = g.players[ee.from].info.id.clone();
            g.add_execution(Exec::Emoji(Box::new(EmojiExec { active: true, from: r, recipient_id: to, recipient: None, emoji: e, random: None })));
        };
        if emoji == "🖕" {
            self.update_relation(r, ee.from, -100.0);
            reply(self, &["🖕", "😡", "🤡", "😞", "😭"], random);
        }
        if emoji == "🤡" {
            self.update_relation(r, ee.from, -10.0);
            reply(self, &["❓", "🤡"], random);
        }
        if ["🕊️", "🏳️", "❤️", "🥰", "👏"].contains(&emoji) {
            if self.config.difficulty == "Easy" {
                self.update_relation(r, ee.from, 15.0);
            }
            let list: &[&str] = if self.relation(ee.from, r) >= 2 { &["❤️", "😊", "🥰"] } else { &["❓", "🤡"] };
            reply(self, list, random);
        }
    }

    // ── NationAllianceBehavior ──

    pub(super) fn nation_handle_alliance_requests(&mut self, ne: &mut NationExec) {
        if self.config.disable_alliances {
            return;
        }
        let p = ne.player.unwrap();
        for r in self.incoming_alliance_requests(p) {
            if self.requests[r].created_at <= self.config.num_spawn_phase_turns() + 1 {
                self.reject_alliance_request(r);
                continue;
            }
            let requestor = self.requests[r].requestor;
            if self.alliance_decision(ne, requestor, true) {
                self.accept_alliance_request(r);
            } else {
                self.reject_alliance_request(r);
            }
        }
    }

    pub(super) fn nation_handle_alliance_extension_requests(&mut self, ne: &mut NationExec) {
        if self.config.disable_alliances {
            return;
        }
        let p = ne.player.unwrap();
        for al in self.players[p].alliances.clone() {
            let a = &self.alliances[al];
            if a.ext_requestor == a.ext_recipient {
                continue;
            }
            let human = a.other(p);
            if !self.alliance_decision(ne, human, true) {
                continue;
            }
            let to_id = self.players[human].info.id.clone();
            self.add_execution(Exec::AllianceExtension { from: p, to_id });
        }
    }

    pub(super) fn maybe_send_alliance_requests(&mut self, ne: &mut NationExec, enemies: &[usize]) {
        if self.config.disable_alliances {
            return;
        }
        let p = ne.player.unwrap();
        let easy = self.config.difficulty == "Easy";
        for &enemy in enemies {
            let acceptable = (self.players[enemy].info.player_type == PlayerType::Bot && easy) || self.players[enemy].info.player_type != PlayerType::Bot;
            if ne.random.next_int(0.0, 30.0) == 0.0
                && acceptable
                && self.can_send_alliance_request(p, enemy)
                && self.alliance_decision(ne, enemy, false)
            {
                let id = self.players[enemy].info.id.clone();
                let e = self.new_alliance_request(p, id);
                self.add_execution(e);
            }
        }
    }

    pub(super) fn alliance_decision(&mut self, ne: &mut NationExec, other: usize, is_response: bool) -> bool {
        let p = ne.player.unwrap();
        self.assert_nation_difficulty();
        let d = self.config.difficulty.clone();
        let (easy, hard, impossible) = (d == "Easy", d == "Hard", d == "Impossible");
        let chance = |ne: &mut NationExec, n: f64| ne.random.next_int(0.0, n) == 0.0;
        // isConfused
        if !impossible && chance(ne, self.by_difficulty([10.0, 20.0, 40.0, f64::INFINITY])) {
            return chance(ne, 2.0);
        }
        if self.is_traitor(other) && ne.random.next_int(0.0, 100.0) >= 10.0 {
            if is_response && chance(ne, 3.0) {
                self.emoji_send(ne, Some(other), &["❓", "🤡"]);
            }
            return false;
        }
        // hasTooManyAlliances (Hard: 50%, Impossible: 25% of the field).
        if hard || impossible {
            let total = self.alive_players().into_iter().filter(|&q| self.players[q].info.player_type != PlayerType::Bot).count() as f64;
            let theirs = self.players[other].alliances.len() as f64 - if self.is_allied_with(other, p) { 1.0 } else { 0.0 };
            let cap = if hard { 0.5 } else { 0.25 };
            if theirs >= total * cap {
                return false;
            }
        }
        if self.find_runaway_leader() == Some(other) {
            return false;
        }
        // isAlliancePartnerThreat: Easy sees none; Medium much more troops;
        // Hard more troops and twice the max troops; Impossible 1.5x troops, max troops, or tiles.
        let threat = match d.as_str() {
            "Easy" => false,
            "Medium" => self.players[other].troops_f() > self.players[p].troops_f() * 2.5,
            "Hard" => self.players[other].troops_f() > self.players[p].troops_f() && self.max_troops(other) > self.max_troops(p) * 2.0,
            _ => {
                let more_troops = self.players[other].troops_f() > self.players[p].troops_f() * 1.5;
                let more_max = self.players[other].troops_f() > self.players[p].troops_f()
                    && self.max_troops(other) > self.max_troops(p) * 1.5;
                let more_tiles = self.players[other].troops_f() > self.players[p].troops_f()
                    && (self.players[other].tiles.len() as f64) > (self.players[p].tiles.len() as f64) * 1.5;
                more_troops || more_max || more_tiles
            }
        };
        if threat {
            if !is_response && chance(ne, 6.0) {
                self.emoji_send(ne, Some(other), &["🙏", "🥺"]);
            }
            if is_response && chance(ne, 6.0) {
                self.emoji_send(ne, Some(other), &["❤️", "😊", "🥰"]);
            }
            return true;
        }
        // shouldRejectInTeamGame
        if self.config.is_team_mode && (impossible || ne.random.next_int(0.0, 100.0) < self.by_difficulty([25.0, 50.0, 75.0, 100.0])) {
            return false;
        }
        if self.relation(p, other) < 2 {
            if is_response && chance(ne, 3.0) {
                self.emoji_send(ne, Some(other), &["❓", "🤡"]);
            }
            return false;
        }
        // isAlliancePartnerFriendly (Hard: 83%, Impossible: 67%)
        let friendly = self.relation(p, other) == 3
            && match d.as_str() {
                "Easy" | "Medium" => true,
                "Hard" => ne.random.next_int(0.0, 100.0) >= 17.0,
                _ => ne.random.next_int(0.0, 100.0) >= 33.0,
            };
        if friendly {
            if chance(ne, 3.0) {
                self.emoji_send(ne, Some(other), &["🤝"]);
            }
            return true;
        }
        // checkAlreadyEnoughAlliances
        let enough = match d.as_str() {
            "Easy" => false,
            "Medium" => self.players[p].alliances.len() as f64 >= ne.random.next_int(4.0, 6.0),
            _ => {
                // Hard & Impossible: don't ally with all our neighbors (when there are 2+).
                let bordering: Vec<usize> = self
                    .nearby(p)
                    .into_iter()
                    .flatten()
                    .filter(|&n| self.players[n].info.player_type != PlayerType::Bot)
                    .collect();
                let friends = bordering.iter().filter(|&&o| self.is_friendly(p, o, false)).count();
                if bordering.len() >= 2 && bordering.contains(&other) {
                    bordering.len() <= friends + 1
                } else if hard {
                    self.players[p].alliances.len() as f64 >= ne.random.next_int(3.0, 5.0)
                } else {
                    self.players[p].alliances.len() as f64 >= ne.random.next_int(2.0, 4.0)
                }
            }
        };
        if enough {
            return false;
        }
        // isEarlygame: Easy accepts 90% in the first 5 minutes, Medium 70% and Hard 50% in the first 3
        let (early, reject) = self.by_difficulty([(3000.0, 10.0), (1800.0, 30.0), (1800.0, 50.0), (600.0, 70.0)]);
        if (self.ticks as f64) < early + self.config.num_spawn_phase_turns() as f64 && ne.random.next_int(0.0, 100.0) >= reject {
            return true;
        }
        // isAlliancePartnerSimilarlyStrong
        let (troop_range, tile_range) = self.by_difficulty([
            ((60.0, 70.0), (70.0, 80.0)),
            ((70.0, 80.0), (80.0, 90.0)),
            ((75.0, 85.0), (85.0, 95.0)),
            ((80.0, 90.0), (90.0, 100.0)),
        ]);
        let _ = easy;
        let my_total = self.players[p].troops_f() + self.outgoing_troops(p);
        let other_total = self.players[other].troops_f() + self.outgoing_troops(other);
        let troop_threshold = my_total * (ne.random.next_int(troop_range.0, troop_range.1) / 100.0);
        let tile_threshold = self.players[p].tiles.len() as f64 * (ne.random.next_int(tile_range.0, tile_range.1) / 100.0);
        other_total > troop_threshold || ((self.players[other].tiles.len() as f64) > tile_threshold && other_total > my_total * 0.5)
    }

    /// NationUtils.findJuiciestTarget: the most structures (not defense posts or
    /// silos), the emptiest troops and the most land, each normalized.
    pub(super) fn find_juiciest_target(&self, candidates: &[usize]) -> Option<usize> {
        if candidates.is_empty() {
            return None;
        }
        let stats: Vec<(usize, f64, f64, f64)> = candidates
            .iter()
            .map(|&q| {
                let structures: u32 = self.players[q]
                    .units
                    .iter()
                    .map(|&u| &self.units[u])
                    .filter(|u| is_structure(u.type_name) && u.type_name != DEFENSE_POST && u.type_name != MISSILE_SILO)
                    .map(|u| u.level)
                    .sum();
                let max = self.max_troops(q);
                let gap = if max > 0.0 { 1.0 - self.players[q].troops_f() / max } else { 0.0 };
                (q, structures as f64, gap, self.players[q].tiles.len() as f64)
            })
            .collect();
        let norm = |v: f64, f: fn(&(usize, f64, f64, f64)) -> f64| {
            let vals: Vec<f64> = stats.iter().map(f).collect();
            let (min, max) = (vals.iter().cloned().fold(f64::INFINITY, f64::min), vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max));
            if max > min { (v - min) / (max - min) } else { 0.0 }
        };
        let mut best = None;
        let mut best_score = f64::NEG_INFINITY;
        for s in &stats {
            let score = norm(s.1, |s| s.1) + norm(s.2, |s| s.2) + norm(s.3, |s| s.3);
            if score > best_score {
                best_score = score;
                best = Some(s.0);
            }
        }
        best
    }

    pub(super) fn outgoing_plus_troops(&self, q: usize) -> f64 {
        self.players[q].troops_f() + self.outgoing_troops(q)
    }

    /// maybeBetray. Hard & Impossible betray their juiciest ally when that's safe; Easy and
    /// Medium betray very weak allies (Easy not humans); all but Easy betray
    /// traitors not much stronger, and a lone neighbor much weaker.
    pub(super) fn maybe_betray(&mut self, ne: &mut NationExec, other: usize, juiciest: Option<usize>, friends: &[usize], enemies: &[usize]) -> bool {
        let p = ne.player.unwrap();
        let Some(al) = self.alliance_with(p, other) else { return false };
        let d = self.config.difficulty.clone();
        let easy = d == "Easy";
        let (mine, theirs) = (self.players[p].troops_f(), self.players[other].troops_f());
        let human = self.players[other].info.player_type == PlayerType::Human;
        let betray = (self.lands_beachheads() && juiciest == Some(other) && self.safe_to_betray(p, other, friends, enemies))
            || (matches!(d.as_str(), "Easy" | "Medium") && !(easy && human) && mine >= theirs * 10.0)
            || (!easy && self.is_traitor(other) && theirs < mine * 1.2)
            || (!easy && friends.len() + enemies.len() == 1 && theirs * 3.0 < mine);
        if betray {
            self.break_alliance(p, al);
        }
        betray
    }

    /// isSafeToBetray: the target, our non-allied neighbors and (unless the
    /// target is a traitor) our other allies hold under a third of our troops.
    pub(super) fn safe_to_betray(&self, p: usize, target: usize, friends: &[usize], enemies: &[usize]) -> bool {
        let others: Vec<usize> = if self.is_traitor(target) {
            Vec::new()
        } else {
            friends.iter().copied().filter(|&f| f != target && self.is_allied_with(p, f)).collect()
        };
        let threat: f64 = std::iter::once(target).chain(enemies.iter().copied()).chain(others).map(|q| self.outgoing_plus_troops(q)).sum();
        threat < self.players[p].troops_f() * 0.33
    }

    // ── Embargoes ──

    pub(super) fn nation_update_relations_from_embargos(&mut self, ne: &mut NationExec) {
        let p = ne.player.unwrap();
        for other in self.alive_players() {
            if other == p {
                continue;
            }
            let manual = self.has_manual_embargo_against(other, p);
            let applied = ne.embargo_malus_applied.contains(&other);
            if manual && !applied {
                self.update_relation(p, other, -20.0);
                ne.embargo_malus_applied.push(other);
            } else if !manual && applied {
                self.update_relation(p, other, 20.0);
                ne.embargo_malus_applied.retain(|&o| o != other);
            }
        }
    }

    pub(super) fn nation_handle_embargoes_to_hostile(&mut self, ne: &mut NationExec) {
        let p = ne.player.unwrap();
        let hard = matches!(self.config.difficulty.as_str(), "Hard" | "Impossible");
        for other in self.alive_players() {
            if other == p {
                continue;
            }
            // Team games, Hard and up: no trade with anyone off the team (bots aside).
            if self.config.is_team_mode && hard && self.players[other].info.player_type != PlayerType::Bot && !self.is_on_same_team(p, other) {
                if !self.has_embargo_against(p, other) {
                    self.add_embargo(p, other, false);
                }
                continue;
            }
            let rel = self.relation(p, other);
            let has = self.has_embargo_against(p, other);
            if rel == 0 && !has && !self.is_on_same_team(p, other) {
                self.add_embargo(p, other, false);
            } else if rel >= 2 && has && !hard {
                self.stop_embargo(p, other);
            } else if rel == 3 && has && self.config.difficulty != "Impossible" {
                self.stop_embargo(p, other);
            }
        }
    }

    // ── NationAttackBehavior and AiAttackBehavior ──

    pub(super) fn nation_force_send_attack(&mut self, ne: &mut NationExec, target: Option<usize>) {
        let p = ne.player.unwrap();
        let id = target.map(|t| self.players[t].info.id.clone());
        let e = self.new_attack(Some(self.players[p].troops_f() / 2.0), p, id);
        self.add_execution(e);
    }

    /// Hard & Impossible boats are beachheads: the moment one lands (its attack
    /// starts at the landing tile), attack its target by land from there, once.
    pub(super) fn nation_follow_up_landings(&mut self, ne: &mut NationExec) {
        if !self.lands_beachheads() {
            return;
        }
        let p = ne.player.unwrap();
        let attacks = self.players[p].outgoing.clone();
        ne.followed_landings.retain(|a| attacks.contains(a));
        for a in attacks {
            let Some(landing) = self.attacks[a].source_tile else { continue };
            if ne.followed_landings.contains(&a) {
                continue;
            }
            ne.followed_landings.push(a);
            let target = self.attacks[a].target;
            if !self.holds_land_next_to(target, landing) {
                continue;
            }
            self.nation_send_land_attack(ne, target, None, target);
        }
    }

    pub(super) fn holds_land_next_to(&self, owner: Neighbor, tile: u32) -> bool {
        let mut nb = [0u32; 4];
        let n = self.map.neighbors4(tile, &mut nb);
        nb[..n].iter().any(|&t| self.map.is_land(t) && !self.map.is_impassable(t) && self.owner_index(t) == owner)
    }

    pub(super) fn lands_beachheads(&self) -> bool {
        matches!(self.config.difficulty.as_str(), "Hard" | "Impossible")
    }

    /// troopSendCap: Hard & Impossible keep back a share of their strongest
    /// hostile neighbor's troops, unless answering incoming attacks.
    pub(super) fn troop_send_cap(&self, p: usize, ignore: Option<usize>) -> f64 {
        let mut cap = self.neighbor_troop_cap(p, ignore);
        let incoming = self.incoming_attacks(p);
        if !incoming.is_empty() {
            cap = cap.max(self.sum_troops(&incoming));
        }
        cap
    }

    pub(super) fn neighbor_troop_cap(&self, p: usize, ignore: Option<usize>) -> f64 {
        if self.config.is_team_mode {
            return f64::INFINITY;
        }
        let retain = match self.config.difficulty.as_str() {
            "Hard" => 0.75,
            "Impossible" => 0.9,
            _ => return f64::INFINITY,
        };
        let mut max = 0.0;
        for n in self.nearby(p).into_iter().flatten() {
            if Some(n) == ignore || self.is_friendly(p, n, false) || self.players[n].info.player_type == PlayerType::Bot {
                continue;
            }
            if self.players[n].troops_f() > max {
                max = self.players[n].troops_f();
            }
        }
        if max == 0.0 {
            return f64::INFINITY;
        }
        (self.players[p].troops_f() - (max * retain).ceil()).max(0.0)
    }

    /// expansionTroopCap: troopSendCap, but free land is throttled, not frozen.
    pub(super) fn expansion_troop_cap(&self, p: usize) -> f64 {
        let cap = self.troop_send_cap(p, None);
        if cap > 0.0 { cap } else { (self.players[p].troops_f() * 0.05).ceil() }
    }

    /// isAttackTooWeak: Hard & Impossible FFA nations don't send under 20% of the target's troops.
    pub(super) fn is_attack_too_weak(&self, p: usize, troops: f64, target: usize) -> bool {
        if self.config.is_team_mode || !self.incoming_attacks(p).is_empty() {
            return false;
        }
        self.lands_beachheads() && troops < self.players[target].troops_f() * 0.2
    }

    /// Hard & Impossible: top up what already attacks the bot, or wait until we can send it all.
    pub(super) fn nation_bot_attack_troops(&self, p: usize, bot: usize, max: f64) -> f64 {
        match self.config.difficulty.as_str() {
            "Easy" => max,
            "Medium" => {
                let mut troops = self.players[bot].troops_f() * 4.0;
                if troops > max {
                    troops = if max < self.players[bot].troops_f() * 2.0 { 0.0 } else { max };
                }
                troops
            }
            _ => {
                let troops = self.bot_attack_troops_needed(p, bot) * 1.3 - self.our_stack_against(p, bot);
                if troops <= max { troops } else { 0.0 }
            }
        }
    }

    /// Keeps the attack at its cheapest loss rate until the bot collapses below 100 tiles.
    pub(super) fn bot_attack_troops_needed(&self, p: usize, bot: usize) -> f64 {
        let stack = (self.players[bot].troops_f() / 0.6).max(1.0);
        let tiles = (self.players[bot].tiles.len() as f64 - 100.0).max(0.0);
        stack.max(tiles * self.bot_tile_loss(p, bot, stack))
    }

    /// Troops lost per tile at that stack; Impossible samples 6x6 grid across
    /// the bot's largest cluster box, while other difficulties assume highland.
    pub(super) fn bot_tile_loss(&self, p: usize, bot: usize, stack: f64) -> f64 {
        self.assert_nation_difficulty();
        let a = &self.players[p];
        let dp = &self.players[bot];
        let mut terrains: Vec<Terrain> = Vec::new();
        if self.config.difficulty == "Impossible" {
            if let Some(b) = dp.largest_cluster_box {
                let width = b[2] - b[0] + 1;
                let height = b[3] - b[1] + 1;
                for i in 0..6 {
                    for j in 0..6 {
                        let x = (b[0] + ((2 * i + 1) * width) / 12) as u32;
                        let y = (b[1] + ((2 * j + 1) * height) / 12) as u32;
                        let tile = self.map.ref_at(x, y);
                        if self.map.owner_id(tile) == dp.small_id {
                            terrains.push(self.map.terrain_type(tile));
                        }
                    }
                }
            }
        }
        if terrains.is_empty() {
            terrains.push(Terrain::Highland);
        }
        let large_attacker_bonus = 1.0 - 0.7 * self.territory_sigmoid_of(a.tiles.len());
        let large_defender_bonus = 1.0 - 0.3 * self.territory_sigmoid_of(dp.tiles.len());
        let traitor_loss_mod = if self.is_traitor(bot) { 0.5 } else { 1.0 };
        let defender_troops = dp.troops_f();
        let defender_loss = defender_troops / dp.tiles.len() as f64;
        let is_bot_vs_player = matches!(a.info.player_type, PlayerType::Human | PlayerType::Nation) && dp.info.player_type == PlayerType::Bot;
        let mut total_loss = 0.0;
        for &terrain in &terrains {
            let mut mag = match terrain {
                Terrain::Plains => 80.0,
                Terrain::Highland => 100.0,
                Terrain::Mountain => 120.0,
                t => panic!("terrain {t:?} cannot be attacked"),
            };
            if is_bot_vs_player {
                mag *= 0.7;
            }
            total_loss += mag
                * traitor_loss_mod
                * within(defender_troops / stack, 0.6, 2.0)
                * (0.463 * large_attacker_bonus * large_defender_bonus + 0.0039 * defender_loss);
        }
        total_loss / terrains.len() as f64
    }

    /// Land attacks on the same target merge, so these troops join the next one,
    /// while the target's attacks on us cancel out against it.
    pub(super) fn our_stack_against(&self, p: usize, target: usize) -> f64 {
        let mut troops = 0.0;
        for &a in &self.players[p].outgoing {
            if self.attacks[a].target == Some(target) && self.attacks[a].source_tile.is_none() {
                troops += self.attacks[a].troops;
            }
        }
        for a in self.incoming_attacks(p) {
            if self.attacks[a].attacker == target {
                troops -= self.attacks[a].troops;
            }
        }
        troops
    }

    /// A 1% boat, if the land attack following its landing would be worth making now.
    pub(super) fn nation_send_beachhead(&mut self, ne: &mut NationExec, target: Neighbor, landing: u32) -> bool {
        let Some(troops) = self.nation_attack_troops(ne, target, None, target) else { return false };
        let p = ne.player.unwrap();
        let boat = (self.players[p].troops_f() / 100.0).floor().max(1.0);
        if target.is_some_and(|t| self.players[t].info.player_type == PlayerType::Bot) {
            ne.bot_attack_troops_sent += boat - troops;
        }
        let e = self.new_transport(p, landing as f64, Some(boat));
        self.add_execution(e);
        true
    }

    pub(super) fn max_boat_route(&self) -> f64 {
        self.by_difficulty([f64::INFINITY, f64::INFINITY, 300.0, 500.0])
    }

    pub(super) fn nation_should_attack(&mut self, ne: &mut NationExec, target: Neighbor) -> bool {
        let Some(o) = target else { return true };
        if self.players[o].info.player_type != PlayerType::Human || self.is_traitor(o) || self.config.humans_vs_nations {
            return true;
        }
        match self.config.difficulty.as_str() {
            "Easy" => ne.random.next_int(0.0, 4.0) == 0.0,
            "Medium" => ne.random.next_int(0.0, 4.0) != 0.0,
            _ => true,
        }
    }

    pub(super) fn nation_send_attack(&mut self, ne: &mut NationExec, target: Neighbor, force: bool) -> bool {
        if !force && !self.nation_should_attack(ne, target) {
            return false;
        }
        let p = ne.player.unwrap();
        match target {
            Some(t) => {
                if self.shares_border_with(p, self.players[t].small_id) {
                    self.nation_send_land_attack(ne, target, None, None)
                } else {
                    self.nation_send_boat_attack(ne, t)
                }
            }
            None => {
                if self.has_land_border_with_terra_nullius(p) {
                    self.nation_send_land_attack(ne, None, None, None)
                } else {
                    self.nation_boat_to_nearby_terra_nullius(ne)
                }
            }
        }
    }

    pub(super) fn nation_find_incoming_attack_player(&self, p: usize) -> Option<usize> {
        let mut largest = 0.0;
        let mut attacker = None;
        for a in self.incoming_attacks(p) {
            let from = self.attacks[a].attacker;
            if self.is_friendly(p, from, false) || self.players[from].info.player_type == PlayerType::Bot {
                continue;
            }
            if self.attacks[a].troops <= largest {
                continue;
            }
            largest = self.attacks[a].troops;
            attacker = Some(from);
        }
        attacker
    }

    /// NationUtils.hasHighStartingGold: from 3M, nations place many structures at once.
    pub(super) fn high_starting_gold(&self, p: usize) -> bool {
        self.config.starting_gold(&self.players[p].info) >= 3_000_000
    }

    pub(super) fn nation_reserve_troops(&self, ne: &NationExec, target: Neighbor) -> f64 {
        let p = ne.player.unwrap();
        // reserveBasis: with high starting gold, cities don't hold back expanding
        // into free land and bots (built all at once, they'd stall it).
        let expansion = target.is_none_or(|t| self.players[t].info.player_type == PlayerType::Bot);
        let max = if expansion && self.high_starting_gold(p) { self.max_troops_without_cities(p) } else { self.max_troops(p) };
        let bot_with_structures = target.is_some_and(|t| {
            self.players[t].info.player_type == PlayerType::Bot && self.players[t].units.iter().any(|&u| is_structure(self.units[u].type_name))
        });
        let use_reserve = target.is_some() && !bot_with_structures;
        max * if use_reserve { ne.reserve_ratio } else { ne.expand_ratio }
    }

    /// calculateAttackTroops (nation override). `send` is the non-bot troop rule.
    /// `opponent` is a player we take on on purpose: it doesn't count toward the
    /// troop cap's threats, and troops already attacking it count toward the too-weak check.
    pub(super) fn nation_attack_troops(&mut self, ne: &mut NationExec, target: Neighbor, send: Option<f64>, opponent: Option<usize>) -> Option<f64> {
        let p = ne.player.unwrap();
        let reserve = self.nation_reserve_troops(ne, target);
        let is_bot = target.is_some_and(|t| self.players[t].info.player_type == PlayerType::Bot);
        let mine = self.players[p].troops_f();
        let mut troops = if is_bot {
            self.nation_bot_attack_troops(p, target.unwrap(), mine - reserve - ne.bot_attack_troops_sent)
        } else {
            send.unwrap_or(mine - reserve)
        };
        let cap = if target.is_some() { self.troop_send_cap(p, opponent) } else { self.expansion_troop_cap(p) };
        troops = troops.min(cap);
        if troops < 1.0 {
            return None;
        }
        if let Some(t) = target {
            let pile_on = if opponent == Some(t) { self.incoming_troops(t) } else { 0.0 };
            if self.is_attack_too_weak(p, troops + pile_on, t) {
                return None;
            }
        }
        if let Some(t) = target {
            self.emoji_maybe_send_attack(ne, t);
        }
        if is_bot {
            ne.bot_attack_troops_sent += troops;
        }
        Some(troops)
    }

    pub(super) fn nation_send_land_attack(&mut self, ne: &mut NationExec, target: Neighbor, send: Option<f64>, opponent: Option<usize>) -> bool {
        let Some(troops) = self.nation_attack_troops(ne, target, send, opponent) else { return false };
        let p = ne.player.unwrap();
        let id = target.map(|t| self.players[t].info.id.clone());
        let e = self.new_attack(Some(troops), p, id);
        self.add_execution(e);
        true
    }

    pub(super) fn hostile_warships(&self, p: usize) -> Vec<usize> {
        let mut out: Vec<usize> = self
            .type_units(WARSHIP)
            .iter()
            .copied()
            .filter(|&u| {
                let unit = &self.units[u];
                unit.active
                    && unit.owner != p
                    && !unit.under_construction
                    && self.ws(u).mode != WarshipMode::Docked
                    && self.can_attack_player_afk(unit.owner, p, true)
            })
            .collect();
        if out.len() > 1 {
            out.sort_by_key(|&u| {
                let owner = self.units[u].owner;
                let pos = self.players[owner].units.iter().position(|&x| x == u).unwrap_or(0);
                (owner, pos)
            });
        }
        out
    }

    /// NationAttackBehavior.sail: Easy doesn't look, Medium checks both shores,
    /// Hard & Impossible pathfind (past 20 tiles).
    pub(super) fn nation_sail(&mut self, p: usize, launch: u32, target: u32, warships: &[usize]) -> (f64, Option<usize>) {
        let landing = self.target_transport_tile(p, target).unwrap_or(target);
        let direct = self.map.manhattan_dist(launch, landing) as f64;
        if self.config.difficulty == "Easy" {
            return (direct, None);
        }
        let path = if self.config.difficulty == "Medium" || direct <= 20.0 {
            None
        } else {
            self.water.find_path(&self.map, &[launch], landing)
        };
        let pathed = path.is_some();
        let route = path.unwrap_or_else(|| vec![launch, landing]);
        let length = if pathed { route.len() as f64 } else { direct };
        let range: i64 = 150;
        let (mut min_x, mut max_x, mut min_y, mut max_y) = (i64::MAX, i64::MIN, i64::MAX, i64::MIN);
        for &t in &route {
            let (x, y) = (self.map.x(t) as i64, self.map.y(t) as i64);
            min_x = min_x.min(x);
            max_x = max_x.max(x);
            min_y = min_y.min(y);
            max_y = max_y.max(y);
        }
        for &w in warships {
            let wt = self.units[w].tile;
            let (wx, wy) = (self.map.x(wt) as i64, self.map.y(wt) as i64);
            if wx < min_x - range || wx > max_x + range || wy < min_y - range || wy > max_y + range {
                continue;
            }
            for (i, &t) in route.iter().enumerate() {
                if self.dist2(wt, t) > range * range {
                    continue;
                }
                // Without a path the warship can fire as soon as we launch.
                let ticks_left = length - if pathed { i as f64 } else { 0.0 };
                if 4.0 * ticks_left >= self.map.manhattan_dist(wt, t) as f64 - 10.0 {
                    return (length, Some(w));
                }
                break;
            }
        }
        (length, None)
    }

    pub(super) fn nation_route_to(&mut self, p: usize, landing: u32, warships: &[usize]) -> Option<BoatRoute> {
        let launch = self.can_build_transport_ship(p, landing)?;
        let (length, blocker) = self.nation_sail(p, launch, landing, warships);
        Some(BoatRoute { landing, length, blocker })
    }

    pub(super) fn nation_boat_route(&mut self, ne: &mut NationExec, target: usize) -> Option<BoatRoute> {
        if let Some(routes) = &ne.boat_routes {
            if let Some(r) = routes.iter().find(|r| r.0 == target) {
                return r.1;
            }
        }
        let route = self.nation_plan_boat_route(ne.player.unwrap(), target);
        if let Some(routes) = &mut ne.boat_routes {
            routes.push((target, route));
        }
        route
    }

    /// AiAttackBehavior.otherLandings: landing spots on their coast apart from
    /// `tried` and clear of hostile warships, nearest to our coast first
    /// (Easy none, Medium 1, Hard 2, Impossible 4).
    pub(super) fn other_landings(&self, ours: &[u32], theirs: &[u32], tried: u32, warships: &[usize]) -> Vec<u32> {
        let count = self.by_difficulty([0, 1, 2, 4]);
        if count == 0 {
            return Vec::new();
        }
        let range2 = 150i64 * 150;
        let our_step = (ours.len() / 32).max(1);
        let our_sample: Vec<u32> = ours.iter().copied().enumerate().filter(|(i, _)| i % our_step == 0).map(|(_, t)| t).collect();
        let mut candidates: Vec<(u32, u32)> = Vec::new();
        let their_step = (theirs.len() / 32).max(1);
        let mut i = 0;
        while i < theirs.len() {
            let tile = theirs[i];
            i += their_step;
            if warships.iter().any(|&w| self.dist2(self.units[w].tile, tile) <= range2) {
                continue;
            }
            let dist = our_sample.iter().map(|&s| self.map.manhattan_dist(s, tile)).min().unwrap_or(u32::MAX);
            candidates.push((tile, dist));
        }
        candidates.sort_by_key(|c| c.1);
        let mut picked = vec![tried];
        for (tile, _) in candidates {
            if picked.len() > count {
                break;
            }
            if picked.iter().any(|&q| self.map.manhattan_dist(q, tile) < 50) {
                continue;
            }
            picked.push(tile);
        }
        picked[1..].to_vec()
    }

    pub(super) fn nation_plan_boat_route(&mut self, p: usize, target: usize) -> Option<BoatRoute> {
        if self.unit_count(p, TRANSPORT_SHIP) >= 3 {
            return None;
        }
        let ours = self.shore_tiles(p);
        let theirs = self.shore_tiles(target);
        let (_, closest) = closest_two_tiles(&self.map, &ours, &theirs)?;
        let warships = self.hostile_warships(p);
        let direct = self.nation_route_to(p, closest, &warships);
        if direct.is_some_and(|r| r.blocker.is_none()) {
            return direct;
        }
        let landings = self.other_landings(&ours, &theirs, closest, &warships);
        let mut best: Option<BoatRoute> = None;
        for landing in landings {
            if let Some(route) = self.nation_route_to(p, landing, &warships) {
                if route.blocker.is_none() && best.is_none_or(|b| route.length < b.length) {
                    best = Some(route);
                }
            }
        }
        best.or(direct)
    }

    pub(super) fn nation_send_boat_attack(&mut self, ne: &mut NationExec, target: usize) -> bool {
        let Some(route) = self.nation_boat_route(ne, target) else { return false };
        let p = ne.player.unwrap();
        if self.lands_beachheads() {
            // A beachhead boat is cheap enough to risk past a warship, one at a time.
            if route.blocker.is_some() && self.unit_count(p, TRANSPORT_SHIP) > 0 {
                return false;
            }
            return self.nation_send_beachhead(ne, Some(target), route.landing);
        }
        if route.blocker.is_some() {
            return false;
        }
        let troops_now = self.players[p].troops_f() / 5.0;
        let Some(troops) = self.nation_attack_troops(ne, Some(target), Some(troops_now), None) else { return false };
        let e = self.new_transport(p, route.landing as f64, Some(troops));
        self.add_execution(e);
        true
    }

    pub(super) fn nation_boat_to_nearby_terra_nullius(&mut self, ne: &mut NationExec) -> bool {
        let p = ne.player.unwrap();
        if self.unit_count(p, TRANSPORT_SHIP) >= 3 {
            return false;
        }
        let shores = self.shore_tiles(p);
        let (w, h) = (self.map.width as i64, self.map.height as i64);
        let mut i = 0;
        while i < shores.len() {
            let border = shores[i];
            i += 10;
            let (bx, by) = (self.map.x(border) as i64, self.map.y(border) as i64);
            for (dx, dy) in [(0i64, -1i64), (0, 1), (-1, 0), (1, 0)] {
                let (x1, y1) = (bx + dx, by + dy);
                if x1 < 0 || y1 < 0 || x1 >= w || y1 >= h || !self.map.is_water(self.map.ref_at(x1 as u32, y1 as u32)) {
                    continue;
                }
                let (nx, ny) = (bx + dx * 5, by + dy * 5);
                if nx < 0 || ny < 0 || nx >= w || ny >= h {
                    continue;
                }
                let tile = self.map.ref_at(nx as u32, ny as u32);
                if !self.map.is_land(tile) || self.map.is_impassable(tile) || self.map.has_owner(tile) || self.map.has_fallout(tile) {
                    continue;
                }
                let Some(launch) = self.can_build_transport_ship(p, tile) else { continue };
                let warships = self.hostile_warships(p);
                if self.nation_sail(p, launch, tile, &warships).1.is_some() {
                    continue;
                }
                // sendExpansionBoat
                if self.lands_beachheads() {
                    return self.nation_send_beachhead(ne, None, tile);
                }
                let troops = (self.players[p].troops_f() / 5.0).min(self.expansion_troop_cap(p));
                if troops < 1.0 {
                    return false;
                }
                let e = self.new_transport(p, tile as f64, Some(troops));
                self.add_execution(e);
                return true;
            }
        }
        false
    }

    pub(super) fn nation_borders_by_land(&self, ne: &NationExec, target: usize) -> bool {
        match &ne.land_neighbors {
            Some(n) => n.contains(&target),
            None => self.shares_border_with(ne.player.unwrap(), self.players[target].small_id),
        }
    }

    pub(super) fn nation_can_reach(&mut self, ne: &mut NationExec, target: usize) -> bool {
        if self.nation_borders_by_land(ne, target) {
            return true;
        }
        let max = self.max_boat_route();
        self.nation_boat_route(ne, target).is_some_and(|r| r.blocker.is_none() && r.length <= max)
    }

    pub(super) fn nation_maybe_attack(&mut self, ne: &mut NationExec) {
        self.assert_nation_difficulty();
        ne.boat_routes = Some(Vec::new());
        self.nation_choose_attack(ne);
        let p = ne.player.unwrap();
        let land_front = ne.land_neighbors.take().unwrap_or_default().iter().any(|&n| !self.is_friendly(p, n, false));
        let routes = ne.boat_routes.take().unwrap_or_default();
        self.nation_clear_blocked_lane(ne, &routes, land_front);
    }

    /// Hard & Impossible send warships at whatever blocks the juiciest target they couldn't reach.
    pub(super) fn nation_clear_blocked_lane(&mut self, ne: &mut NationExec, routes: &[(usize, Option<BoatRoute>)], land_front: bool) {
        if !self.lands_beachheads() {
            return;
        }
        let p = ne.player.unwrap();
        let max = self.max_boat_route();
        let ffa = !self.config.is_team_mode;
        let mut blocked: Vec<(usize, usize)> = Vec::new();
        for &(target, route) in routes {
            let Some(route) = route else { continue };
            let Some(blocker) = route.blocker else { continue };
            if (!land_front || route.length <= max) && (!ffa || self.players[target].troops_f() < self.players[p].troops_f()) {
                blocked.push((target, blocker));
            }
        }
        let keys: Vec<usize> = blocked.iter().map(|b| b.0).collect();
        if let Some(target) = self.find_juiciest_target(&keys) {
            let blocker = blocked.iter().find(|b| b.0 == target).unwrap().1;
            self.clear_sea_lane(p, blocker);
        }
    }

    pub(super) fn nation_holds_boats_under_attack(&mut self, ne: &mut NationExec) -> bool {
        if self.nation_find_incoming_attack_player(ne.player.unwrap()).is_none() {
            return false;
        }
        // Easy never holds its boats back; Medium half the time; Hard & Impossible always.
        match self.config.difficulty.as_str() {
            "Easy" => false,
            "Medium" => ne.random.next_int(0.0, 2.0) == 0.0,
            _ => true,
        }
    }

    pub(super) fn nation_choose_attack(&mut self, ne: &mut NationExec) {
        let p = ne.player.unwrap();
        let small = self.players[p].small_id;
        let mut bordering: Vec<usize> = Vec::new();
        let mut border_tn = false;
        let mut nb = [0u32; 4];
        for t in self.players[p].border_tiles.values() {
            let n = self.map.neighbors4(t, &mut nb);
            for &x in &nb[..n] {
                if !self.map.is_land(x) || self.map.is_impassable(x) || self.map.owner_id(x) == small {
                    continue;
                }
                if let Some(o) = self.owner_index(x) {
                    if !bordering.contains(&o) {
                        bordering.push(o);
                    }
                }
                if !self.map.has_owner(x) && !self.map.has_fallout(x) {
                    border_tn = true;
                }
            }
        }
        ne.land_neighbors = Some(bordering.clone());
        let nearby = self.nearby(p);
        for n in nearby.iter().flatten() {
            if !bordering.contains(n) {
                bordering.push(*n);
            }
        }
        bordering.sort_by(|&a, &b| self.players[a].troops.cmp(&self.players[b].troops));
        let friends: Vec<usize> = bordering.iter().copied().filter(|&o| self.is_friendly(p, o, false)).collect();
        let enemies: Vec<usize> = bordering.iter().copied().filter(|&o| !self.is_friendly(p, o, false)).collect();
        if (border_tn || nearby.iter().any(|n| n.is_none())) && self.nation_send_attack(ne, None, false) {
            return;
        }
        let hold = self.nation_holds_boats_under_attack(ne);
        if enemies.is_empty() {
            if !hold && ne.random.next_int(0.0, 5.0) == 0.0 {
                self.nation_attack_with_random_boat(ne, &[]);
            }
        } else {
            if !hold && ne.random.next_int(0.0, 10.0) == 0.0 {
                self.nation_attack_with_random_boat(ne, &enemies);
                return;
            }
            self.maybe_send_alliance_requests(ne, &enemies);
        }
        self.nation_attack_best_target(ne, &friends, &enemies);
    }

    pub(super) fn nation_attack_with_random_boat(&mut self, ne: &mut NationExec, enemies: &[usize]) {
        let p = ne.player.unwrap();
        if self.unit_count(p, TRANSPORT_SHIP) >= 3 {
            return;
        }
        let shore = self.shore_tiles(p);
        if shore.is_empty() {
            return;
        }
        let src = shore[ne.random.next_int(0.0, shore.len() as f64) as usize];
        let found = match self.nation_find_random_boat_target(ne, src, enemies, true) {
            Some(f) => f,
            None => match self.nation_find_random_boat_target(ne, src, enemies, false) {
                Some(f) => f,
                None => return,
            },
        };
        let (dst, launch) = found;
        let warships = self.hostile_warships(p);
        if self.nation_sail(p, launch, dst, &warships).1.is_some() {
            return;
        }
        let owner = self.owner_index(dst);
        if self.lands_beachheads() {
            self.nation_send_beachhead(ne, owner, dst);
            return;
        }
        let cap = if owner.is_some() { self.troop_send_cap(p, None) } else { self.expansion_troop_cap(p) };
        let troops = (self.players[p].troops_f() / 5.0).min(cap);
        if troops < 1.0 {
            return;
        }
        if owner.is_some_and(|o| self.is_attack_too_weak(p, troops, o)) {
            return;
        }
        let e = self.new_transport(p, dst as f64, Some(troops));
        self.add_execution(e);
    }

    pub(super) fn nation_find_random_boat_target(&mut self, ne: &mut NationExec, tile: u32, enemies: &[usize], high_interest: bool) -> Option<(u32, u32)> {
        let p = ne.player.unwrap();
        let (x, y) = (self.map.x(tile) as f64, self.map.y(tile) as f64);
        let (w, h) = (self.map.width as f64, self.map.height as f64);
        let mut unreachable: Vec<usize> = Vec::new();
        let ffa = !self.config.is_team_mode;
        for _ in 0..500 {
            let rx = ne.random.next_int(x - 150.0, x + 150.0);
            let ry = ne.random.next_int(y - 150.0, y + 150.0);
            if !(rx >= 0.0 && rx < w && ry >= 0.0 && ry < h) {
                continue;
            }
            let t = self.map.ref_at(rx as u32, ry as u32);
            if !self.map.is_land(t) || self.map.is_impassable(t) {
                continue;
            }
            let owner = self.owner_index(t);
            if owner == Some(p) {
                continue;
            }
            if let Some(o) = owner {
                if unreachable.contains(&o) || enemies.contains(&o) {
                    continue;
                }
                if ffa && self.players[o].troops > self.players[p].troops {
                    continue;
                }
            }
            let matches = match owner {
                None => true,
                Some(o) => {
                    if high_interest {
                        self.players[o].info.player_type == PlayerType::Bot
                    } else {
                        !self.is_friendly(o, p, false)
                    }
                }
            };
            if !matches {
                continue;
            }
            let Some(launch) = self.can_build_transport_ship(p, t) else {
                if let Some(o) = owner {
                    unreachable.push(o);
                }
                continue;
            };
            return Some((t, launch));
        }
        None
    }

    pub(super) fn nation_attack_best_target(&mut self, ne: &mut NationExec, friends: &[usize], enemies: &[usize]) {
        let p = ne.player.unwrap();
        let d = self.config.difficulty.clone();
        // Hard & Impossible: answer attacks right away, even below the reserve and trigger ratios.
        if self.lands_beachheads() && self.nation_retaliate(ne) {
            return;
        }
        self.nation_annex_walled_in_bots(ne);
        // Bots go first when they hold structures, and with high starting gold
        // (measured without the cities built in a rush).
        let expand_first = self.high_starting_gold(p)
            && self.players[p].troops_f() / self.max_troops_without_cities(p) >= ne.trigger_ratio;
        if (self.nation_has_neighboring_bot_with_structures(p) || expand_first) && self.nation_attack_bots(ne) {
            return;
        }
        // hasReserveRatioTroops
        if self.players[p].troops_f() / self.max_troops(p) < ne.reserve_ratio {
            return;
        }
        // Medium answers attacks without saving up to the trigger ratio first.
        if d == "Medium" && self.nation_retaliate(ne) {
            return;
        }
        let trigger = self.players[p].troops_f() / self.max_troops(p) >= ne.trigger_ratio;
        if !trigger && ne.random.next_int(0.0, 10.0) != 0.0 {
            return;
        }
        let order: &[&str] = match d.as_str() {
            "Easy" => &["nuked", "bots", "retaliate", "assist", "betray", "hated", "weakest"],
            "Medium" => &["bots", "nuked", "assist", "betray", "hated", "afk", "traitor", "crown", "weakest", "island", "donate"],
            "Hard" => &[
                "bots", "assist", "betray", "nuked", "traitor", "afk", "hated", "veryWeak", "juicy", "victim", "crown", "weakest", "island",
                "donate",
            ],
            _ => &[
                "bots", "veryWeak", "betray", "assist", "victim", "crown", "traitor", "juicy", "afk", "nuked", "hated", "weakest", "island",
                "donate",
            ],
        };
        let mut groups: Option<Vec<Vec<usize>>> = None;
        for &strategy in order {
            if self.nation_strategy(ne, strategy, &mut groups, friends, enemies) {
                return;
            }
        }
    }

    pub(super) fn nation_strategy(
        &mut self,
        ne: &mut NationExec,
        strategy: &str,
        groups: &mut Option<Vec<Vec<usize>>>,
        friends: &[usize],
        enemies: &[usize],
    ) -> bool {
        let p = ne.player.unwrap();
        let ffa = !self.config.is_team_mode;
        let my = self.players[p].troops_f();
        let attack = |g: &mut Game, ne: &mut NationExec, t: Option<usize>| t.is_some_and(|t| g.nation_send_attack(ne, Some(t), false));
        match strategy {
            "retaliate" => self.nation_retaliate(ne),
            "bots" => self.nation_attack_bots(ne),
            "assist" => self.nation_assist_allies(ne),
            "betray" => self.nation_betray_and_attack(ne, friends, enemies),
            "hated" => self.nation_attack_hated(ne),
            "nuked" => self.nation_bordering_nuked_territory(p) && self.nation_send_attack(ne, None, false),
            "traitor" => {
                if self.config.disable_alliances {
                    return false;
                }
                let t = self.nation_pick_in(ne, groups, enemies, |g, e| {
                    e.iter().copied().find(|&x| g.is_traitor(x) && (!ffa || g.players[x].troops_f() < my * 1.2))
                });
                attack(self, ne, t)
            }
            "afk" => {
                let t = self.nation_pick_in(ne, groups, enemies, |g, e| {
                    e.iter().copied().find(|&x| g.players[x].is_disconnected && (!ffa || g.players[x].troops_f() < my * 3.0))
                });
                attack(self, ne, t)
            }
            "veryWeak" => {
                let t = self.nation_pick_in(ne, groups, enemies, |g, e| {
                    e.iter().copied().find(|&x| {
                        g.players[x].troops_f() < g.max_troops(x) * 0.15 && (!ffa || g.players[x].troops_f() < my * 1.2)
                    })
                });
                attack(self, ne, t)
            }
            "juicy" => {
                let t = self.nation_pick_in(ne, groups, enemies, |g, e| {
                    let candidates: Vec<usize> = e.iter().copied().filter(|&x| g.players[x].troops_f() <= my * 0.75).collect();
                    g.find_juiciest_target(&candidates)
                });
                attack(self, ne, t)
            }
            "victim" => {
                let t = self.nation_pick_in(ne, groups, enemies, |g, e| {
                    e.iter().copied().find(|&x| {
                        !(ffa && g.players[x].troops_f() > my * 1.2) && g.incoming_troops(x) > g.players[x].troops_f() * 0.5
                    })
                });
                attack(self, ne, t)
            }
            "crown" => match self.nation_find_crown_target(p, enemies) {
                Some(leader) => self.nation_should_attack(ne, Some(leader)) && self.nation_send_land_attack(ne, Some(leader), None, Some(leader)),
                None => false,
            },
            "weakest" => {
                let t = self.nation_pick_in(ne, groups, enemies, |g, e| e.iter().copied().find(|&x| !ffa || g.players[x].troops_f() < my));
                attack(self, ne, t)
            }
            "island" => {
                if self.nation_pick_in(ne, groups, enemies, |_, e| e.first().copied()).is_some() {
                    return false;
                }
                let enemy = self.nation_find_nearest_island_enemy(ne);
                attack(self, ne, enemy)
            }
            "donate" => self.nation_donate_troops(ne),
            _ => unreachable!(),
        }
    }

    /// donateTroops: team games, not public ones (to balance HvN).
    pub(super) fn nation_donate_troops(&mut self, ne: &mut NationExec) -> bool {
        if !self.config.is_team_mode || self.config.is_public || !self.config.donate_troops || self.config.difficulty == "Easy" {
            return false;
        }
        match self.config.difficulty.as_str() {
            "Medium" => {
                if ne.random.next_int(0.0, 4.0) != 0.0 {
                    return false;
                }
            }
            "Hard" => {
                if ne.random.next_int(0.0, 2.0) != 0.0 {
                    return false;
                }
            }
            _ => {}
        }
        let p = ne.player.unwrap();
        let mut candidates: Vec<(usize, f64)> = self
            .alive_players()
            .into_iter()
            .filter(|&q| {
                self.is_on_same_team(p, q)
                    && (!self.incoming_attacks(q).is_empty() || !self.players[q].outgoing.is_empty())
            })
            .map(|q| (q, self.players[q].troops_f() / self.max_troops(q).max(1.0)))
            .collect();
        if candidates.is_empty() {
            return false;
        }
        candidates.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        let Some((selected, _)) = candidates.into_iter().find(|&(q, _)| self.can_donate(p, q, false)) else {
            return false;
        };
        let available = self.players[p].troops_f() - self.max_troops(p) * ne.reserve_ratio;
        if available < 1.0 {
            return false;
        }
        let id = self.players[selected].info.id.clone();
        let e = self.new_donation(p, false, id, Some(available));
        self.add_execution(e);
        true
    }

    /// Target groups, tried in order: Hard & Impossible without a land front weigh
    /// every enemy within boat range across the water alongside the bordering ones.
    pub(super) fn nation_target_groups(&mut self, ne: &mut NationExec, enemies: &[usize]) -> Vec<Vec<usize>> {
        let p = ne.player.unwrap();
        let land_front = enemies.iter().any(|&e| self.nation_borders_by_land(ne, e));
        let by_troops = |g: &Game, v: &mut Vec<usize>| v.sort_by(|&a, &b| g.players[a].troops.cmp(&g.players[b].troops));
        match self.config.difficulty.as_str() {
            "Impossible" if land_front => {
                let mut over = self.overseas_targets(p, enemies);
                by_troops(self, &mut over);
                vec![enemies.to_vec(), over]
            }
            "Hard" | "Impossible" if !land_front => {
                let mut all = enemies.to_vec();
                all.extend(self.overseas_targets(p, enemies));
                by_troops(self, &mut all);
                vec![all]
            }
            _ => vec![enemies.to_vec()],
        }
    }

    /// Enemies across the water whose main territory is within boat range of ours.
    pub(super) fn overseas_targets(&self, p: usize, enemies: &[usize]) -> Vec<usize> {
        if self.unit_count(p, TRANSPORT_SHIP) >= 3 {
            return Vec::new();
        }
        let range = self.max_boat_route();
        let ours = self.players[p].largest_cluster_box;
        let ffa = !self.config.is_team_mode;
        let gap = |a: Option<[i64; 4]>, b: Option<[i64; 4]>| -> f64 {
            let (Some(a), Some(b)) = (a, b) else { return 0.0 };
            let dx = 0.max(a[0] - b[2]).max(b[0] - a[2]);
            let dy = 0.max(a[1] - b[3]).max(b[1] - a[3]);
            (dx + dy) as f64
        };
        self.alive_players()
            .into_iter()
            .filter(|&q| {
                q != p
                    && self.players[q].info.player_type != PlayerType::Bot
                    && !self.is_friendly(p, q, false)
                    && !enemies.contains(&q)
                    && (!ffa || self.players[q].troops_f() < self.players[p].troops_f())
                    && gap(ours, self.players[q].largest_cluster_box) <= range
            })
            .collect()
    }

    /// getAttackStrategies' pick over the target groups.
    pub(super) fn nation_pick_in(
        &mut self,
        ne: &mut NationExec,
        groups: &mut Option<Vec<Vec<usize>>>,
        enemies: &[usize],
        find: impl Fn(&Game, &[usize]) -> Option<usize>,
    ) -> Option<usize> {
        if groups.is_none() {
            *groups = Some(self.nation_target_groups(ne, enemies));
        }
        for group in groups.clone().unwrap() {
            if let Some(t) = self.nation_pick(ne, &group, &find) {
                return Some(t);
            }
        }
        None
    }

    /// maybeBetrayAndAttack: betray the first friend worth betraying and attack it.
    pub(super) fn nation_betray_and_attack(&mut self, ne: &mut NationExec, friends: &[usize], enemies: &[usize]) -> bool {
        if self.config.disable_alliances {
            return false;
        }
        // findJuiciestAlly: once per call, not per friend.
        let allies: Vec<usize> = friends.iter().copied().filter(|&f| self.is_allied_with(ne.player.unwrap(), f)).collect();
        let juiciest = self.find_juiciest_target(&allies);
        for &friend in friends {
            if self.maybe_betray(ne, friend, juiciest, friends, enemies) {
                return self.nation_send_attack(ne, Some(friend), true);
            }
        }
        false
    }

    /// getAttackStrategies' pick: what `find` picks among the targets we can reach.
    pub(super) fn nation_pick(&mut self, ne: &mut NationExec, group: &[usize], find: impl Fn(&Game, &[usize]) -> Option<usize>) -> Option<usize> {
        let mut pool = group.to_vec();
        loop {
            let target = find(self, &pool)?;
            if self.config.difficulty == "Easy" || self.nation_can_reach(ne, target) {
                return Some(target);
            }
            pool.retain(|&x| x != target);
        }
    }

    pub(super) fn nation_retaliate(&mut self, ne: &mut NationExec) -> bool {
        let p = ne.player.unwrap();
        let Some(attacker) = self.nation_find_incoming_attack_player(p) else { return false };
        if self.lands_beachheads() && !self.config.is_team_mode && self.shares_border_with(p, self.players[attacker].small_id) {
            return self.nation_send_retaliation(ne, attacker);
        }
        self.nation_send_attack(ne, Some(attacker), true)
    }

    /// sendRetaliation: cancel the attacker's incoming troops, and push into its
    /// land when we can also match its home army.
    pub(super) fn nation_send_retaliation(&mut self, ne: &mut NationExec, attacker: usize) -> bool {
        let p = ne.player.unwrap();
        let incoming: f64 = self.incoming_attacks(p).iter().filter(|&&a| self.attacks[a].attacker == attacker).map(|&a| self.attacks[a].troops).sum();
        let pressing: f64 =
            self.players[p].outgoing.iter().filter(|&&a| self.attacks[a].target == Some(attacker)).map(|&a| self.attacks[a].troops).sum();
        let troops = self.players[p].troops_f();
        let max = self.max_troops(p);
        let spare = (troops - max * ne.expand_ratio).min(self.neighbor_troop_cap(p, Some(attacker)));
        let above_reserve = troops - max * ne.reserve_ratio;
        let counter = incoming + (self.players[attacker].troops_f() - pressing).max(0.0);
        let send = if spare >= counter { counter.max(above_reserve.min(spare)) } else { incoming.min(spare.max(above_reserve)) };
        self.nation_send_land_attack(ne, Some(attacker), Some(send), Some(attacker))
    }

    pub(super) fn nation_neighboring_bots(&self, p: usize) -> Vec<usize> {
        self.nearby(p)
            .into_iter()
            .flatten()
            .filter(|&n| !self.is_friendly(p, n, false) && self.players[n].info.player_type == PlayerType::Bot)
            .collect()
    }

    /// annexWalledInBots: a 1% attack makes the engine hand over a bot only we border.
    pub(super) fn nation_annex_walled_in_bots(&mut self, ne: &mut NationExec) {
        if !self.lands_beachheads() {
            return;
        }
        let p = ne.player.unwrap();
        let bots = self.nation_neighboring_bots(p);
        if bots.is_empty() {
            return;
        }
        let troops = (self.players[p].troops_f() / 100.0).floor();
        if troops < 1.0 {
            return;
        }
        for bot in self.walled_in_bots(p, &bots) {
            let e = self.new_attack(Some(troops), p, Some(self.players[bot].info.id.clone()));
            self.add_execution(e);
        }
    }

    pub(super) fn attacked_by(&self, p: usize) -> Vec<usize> {
        let mut out = Vec::new();
        for &a in &self.players[p].outgoing {
            if let Some(t) = self.attacks[a].target {
                if !out.contains(&t) {
                    out.push(t);
                }
            }
        }
        out
    }

    pub(super) fn owns_structures(&self, p: usize) -> bool {
        self.players[p].units.iter().any(|&u| is_structure(self.units[u].type_name))
    }

    /// BotAnnexation.walledInBots: bots that border only us and aren't under our attack yet.
    pub(super) fn walled_in_bots(&self, p: usize, bots: &[usize]) -> Vec<usize> {
        let attacking = self.attacked_by(p);
        bots.iter()
            .copied()
            .filter(|&b| !attacking.contains(&b) && self.bot_scan(p, b, true, Some(&[])).is_some_and(|s| s.borders_us))
            .collect()
    }

    /// BotScanner.scan: what a bot's territory touches. Gives up (None) once the bot
    /// turns out open if `closed_only`, or borders a bot outside `within`.
    pub(super) fn bot_scan(&self, p: usize, bot: usize, closed_only: bool, within: Option<&[usize]>) -> Option<BotSurroundings> {
        let us = self.players[p].small_id;
        let me = self.players[bot].small_id;
        let mut s = BotSurroundings { bot, borders_us: false, open: false, bots: Vec::new() };
        let mut nb = [0u32; 4];
        for tile in self.players[bot].border_tiles.values() {
            if self.map.is_ocean_shore(tile) || self.map.is_on_edge_of_map(tile) {
                s.open = true;
                if closed_only {
                    return None;
                }
                continue;
            }
            let n = self.map.neighbors4(tile, &mut nb);
            for &t in &nb[..n] {
                let id = self.map.owner_id(t);
                if id == me {
                    continue;
                }
                if id == us {
                    s.borders_us = true;
                    continue;
                }
                let other = if id == 0 {
                    None
                } else {
                    self.owner_index(t).filter(|&o| self.players[o].info.player_type == PlayerType::Bot && !self.is_friendly(p, o, false))
                };
                match other {
                    None => {
                        s.open = true;
                        if closed_only {
                            return None;
                        }
                    }
                    Some(o) => {
                        if within.is_some_and(|w| !w.contains(&o)) {
                            return None;
                        }
                        if !s.bots.contains(&o) {
                            s.bots.push(o);
                        }
                    }
                }
            }
        }
        Some(s)
    }

    /// BotAnnexation.planBotAttacks: a checkerboard of bots across our front and the
    /// row behind it is left to be annexed for free; the bots walling them in go first.
    pub(super) fn plan_bot_attacks(&self, p: usize, bots: &[usize]) -> Vec<usize> {
        let mut region: Vec<BotSurroundings> = bots.iter().map(|&b| self.bot_scan(p, b, false, None).unwrap()).collect();
        let get = |region: &Vec<BotSurroundings>, b: usize| region.iter().position(|s| s.bot == b);
        let front: Vec<usize> = bots.iter().copied().filter(|&b| region[get(&region, b).unwrap()].borders_us).collect();
        let mut behind: Vec<usize> = Vec::new();
        for &bot in &front {
            let others = region[get(&region, bot).unwrap()].bots.clone();
            for other in others {
                if get(&region, other).is_some() || behind.contains(&other) {
                    continue;
                }
                behind.push(other);
                if let Some(s) = self.bot_scan(p, other, true, None) {
                    region.push(s);
                }
            }
        }
        let attacking = self.attacked_by(p);
        let mut candidates: Vec<&BotSurroundings> = region
            .iter()
            .filter(|s| !s.open && !s.bots.is_empty() && !attacking.contains(&s.bot) && !self.owns_structures(s.bot))
            .collect();
        candidates.sort_by(|a, b| self.players[b.bot].tiles.len().cmp(&self.players[a.bot].tiles.len()));
        let mut kept: Vec<usize> = Vec::new();
        let mut next_to_kept: Vec<usize> = Vec::new();
        let mut walled_in: Vec<(usize, f64)> = Vec::new();
        for s in candidates {
            if next_to_kept.contains(&s.bot) {
                continue;
            }
            kept.push(s.bot);
            let share = self.players[s.bot].tiles.len() as f64 / s.bots.len() as f64;
            for &other in &s.bots {
                if !next_to_kept.contains(&other) {
                    next_to_kept.push(other);
                }
                match walled_in.iter_mut().find(|w| w.0 == other) {
                    Some(w) => w.1 += share,
                    None => walled_in.push((other, share)),
                }
            }
        }
        let value = |b: usize| {
            let extra = walled_in.iter().find(|w| w.0 == b).map_or(0.0, |w| w.1);
            (self.players[b].tiles.len() as f64 + extra) / self.players[b].troops_f().max(1.0)
        };
        let by_value = |a: &usize, b: &usize| value(*b).partial_cmp(&value(*a)).unwrap_or(std::cmp::Ordering::Equal);
        let mut targets: Vec<usize> = bots
            .iter()
            .copied()
            .filter(|&b| {
                let s = &region[get(&region, b).unwrap()];
                let walled = s.borders_us && !s.open && s.bots.is_empty();
                !kept.contains(&b) && !walled
            })
            .collect();
        targets.sort_by(by_value);
        if !targets.iter().any(|&b| region[get(&region, b).unwrap()].borders_us) {
            let mut more: Vec<usize> = front.iter().copied().filter(|b| kept.contains(b)).collect();
            more.sort_by(by_value);
            targets.extend(more);
        }
        targets
    }

    pub(super) fn nation_has_neighboring_bot_with_structures(&self, p: usize) -> bool {
        self.nearby(p).into_iter().flatten().any(|n| {
            self.players[n].info.player_type == PlayerType::Bot
                && !self.is_friendly(p, n, false)
                && self.players[n].units.iter().any(|&u| is_structure(self.units[u].type_name))
        })
    }

    pub(super) fn nation_attack_bots(&mut self, ne: &mut NationExec) -> bool {
        let p = ne.player.unwrap();
        let mut bots = self.nation_neighboring_bots(p);
        if bots.is_empty() {
            return false;
        }
        ne.bot_attack_troops_sent = 0.0;
        let annexes = self.lands_beachheads();
        if annexes {
            bots = self.plan_bot_attacks(p, &bots);
        } else {
            let density = |g: &Game, b: usize| g.players[b].troops_f() / g.players[b].tiles.len() as f64;
            bots.sort_by(|&a, &b| density(self, a).partial_cmp(&density(self, b)).unwrap_or(std::cmp::Ordering::Equal));
        }
        bots.sort_by(|&a, &b| (self.owns_structures(b) as u8).cmp(&(self.owns_structures(a) as u8)));
        // getBotAttackMaxParallelism
        let n = match self.config.difficulty.as_str() {
            "Easy" => 1,
            "Medium" => if ne.random.next_int(0.0, 2.0) == 0.0 { 1 } else { 2 },
            "Hard" => 3,
            _ => 100,
        };
        let reduced: Vec<usize> = bots.into_iter().take(n).collect();
        for &bot in &reduced {
            self.nation_send_attack(ne, Some(bot), false);
        }
        // Hard & Impossible only top up running attacks, so those count as well.
        ne.bot_attack_troops_sent > 0.0
            || (annexes && self.players[p].outgoing.iter().any(|&a| self.attacks[a].target.is_some_and(|t| reduced.contains(&t))))
    }

    pub(super) fn nation_bordering_nuked_territory(&self, p: usize) -> bool {
        let mut nb = [0u32; 4];
        for t in self.players[p].border_tiles.values() {
            let n = self.map.neighbors4(t, &mut nb);
            if nb[..n].iter().any(|&x| self.map.is_land(x) && !self.map.has_owner(x) && self.map.has_fallout(x)) {
                return true;
            }
        }
        false
    }

    pub(super) fn nation_assist_allies(&mut self, ne: &mut NationExec) -> bool {
        if self.config.disable_alliances {
            return false;
        }
        let p = ne.player.unwrap();
        for ally in self.allies(p) {
            let targets = self.targets(ally);
            if targets.is_empty() {
                continue;
            }
            if self.relation(p, ally) < 3 {
                self.emoji_send(ne, Some(ally), &["🥱", "🤦‍♂️"]);
                continue;
            }
            for target in targets {
                if target == p {
                    self.emoji_send(ne, Some(ally), &["🥺", "💀"]);
                    continue;
                }
                if self.is_friendly(p, target, false) {
                    self.emoji_send(ne, Some(ally), &["🕊️", "👎"]);
                    continue;
                }
                if !self.nation_send_attack(ne, Some(target), false) {
                    continue;
                }
                self.update_relation(p, ally, -20.0);
                self.emoji_send(ne, Some(ally), &["👍", "🤝", "🎯"]);
                return true;
            }
        }
        false
    }

    pub(super) fn nation_attack_hated(&mut self, ne: &mut NationExec) -> bool {
        let p = ne.player.unwrap();
        // allRelationsSorted
        let mut rels: Vec<(usize, f64)> = self.players[p].relations.iter().copied().filter(|r| self.players[r.0].is_alive()).collect();
        rels.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        let ffa = !self.config.is_team_mode;
        for (other, v) in rels {
            if relation_from_value(v) != 0 || self.is_friendly(p, other, false) {
                continue;
            }
            if ffa && self.players[other].troops_f() > self.players[p].troops_f() * 3.0 {
                continue;
            }
            if self.config.difficulty != "Easy" && !self.nation_can_reach(ne, other) {
                continue;
            }
            return self.nation_send_attack(ne, Some(other), false);
        }
        false
    }

    pub(super) fn nation_find_crown_target(&self, p: usize, enemies: &[usize]) -> Option<usize> {
        let leader = self.find_runaway_leader()?;
        if !enemies.contains(&leader) || !self.shares_border_with(p, self.players[leader].small_id) {
            return None;
        }
        if matches!(self.config.difficulty.as_str(), "Hard" | "Impossible") {
            return Some(leader);
        }
        if self.incoming_troops(leader) >= self.players[leader].troops_f() * 0.1 {
            Some(leader)
        } else {
            None
        }
    }

    pub(super) fn nation_find_nearest_island_enemy(&mut self, ne: &mut NationExec) -> Option<usize> {
        let p = ne.player.unwrap();
        if self.unit_count(p, TRANSPORT_SHIP) >= 3 {
            return None;
        }
        if !self.players[p].border_tiles.values().any(|t| self.map.is_shore(t)) {
            return None;
        }
        let ffa = !self.config.is_team_mode;
        let my = self.players[p].troops_f();
        let candidates: Vec<usize> = self
            .alive_players()
            .into_iter()
            .filter(|&q| q != p && !self.is_friendly(p, q, false) && (!ffa || self.players[q].troops_f() < my))
            .collect();
        if candidates.is_empty() {
            return None;
        }
        let (cx, cy) = self.bounding_box_center(p);
        let center = self.map.ref_at(cx as u32, cy as u32);
        let mut sorted: Vec<(usize, u32)> = candidates
            .into_iter()
            .map(|q| {
                let (qx, qy) = self.bounding_box_center(q);
                (q, self.map.manhattan_dist(center, self.map.ref_at(qx as u32, qy as u32)))
            })
            .collect();
        sorted.sort_by_key(|e| e.1);
        let mut reachable = Vec::new();
        for (q, _) in sorted {
            // Beachhead boats sail any distance, even past a warship: nothing nearer is left.
            let ok = if self.lands_beachheads() {
                self.nation_borders_by_land(ne, q) || self.nation_boat_route(ne, q).is_some()
            } else {
                self.nation_can_reach(ne, q)
            };
            if ok {
                reachable.push(q);
                if reachable.len() >= 2 {
                    break;
                }
            }
        }
        if reachable.is_empty() {
            return None;
        }
        if reachable.len() >= 2 && ne.random.next_int(0.0, 3.0) == 0.0 {
            return Some(reachable[1]);
        }
        Some(reachable[0])
    }

    // ── Hard & Impossible only (Medium nations never do these) ──


}

