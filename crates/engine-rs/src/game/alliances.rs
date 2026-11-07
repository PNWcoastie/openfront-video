use super::*;
use super::nation_attack::emoji_ids;

// ── Alliances (AllianceImpl, AllianceRequestImpl, alliance executions) ──

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RequestStatus {
    Pending,
    Accepted,
    Rejected,
}

pub struct AllianceRequest {
    pub requestor: usize,
    pub recipient: usize,
    pub created_at: u32,
    pub status: RequestStatus,
}

pub struct Alliance {
    pub id: u32,
    pub requestor: usize,
    pub recipient: usize,
    pub created_at: u32,
    pub expires_at: u32,
    pub ext_requestor: bool,
    pub ext_recipient: bool,
}

impl Alliance {
    pub fn other(&self, p: usize) -> usize {
        if self.requestor == p {
            self.recipient
        } else {
            self.requestor
        }
    }
}

pub struct AllianceRequestExec {
    pub(super) active: bool,
    pub(super) requestor: usize,
    pub(super) recipient_id: String,
    pub(super) req: Option<usize>,
}

impl Game {
    pub fn new_alliance_request(&self, requestor: usize, recipient_id: String) -> Exec {
        Exec::AllianceRequest(Box::new(AllianceRequestExec { active: true, requestor, recipient_id, req: None }))
    }

    pub fn incoming_alliance_requests(&self, p: usize) -> Vec<usize> {
        self.alliance_requests.iter().copied().filter(|&r| self.requests[r].recipient == p).collect()
    }

    pub fn outgoing_alliance_requests(&self, p: usize) -> Vec<usize> {
        self.alliance_requests.iter().copied().filter(|&r| self.requests[r].requestor == p).collect()
    }

    pub(super) fn alliance_with(&self, a: usize, b: usize) -> Option<usize> {
        if a == b {
            return None;
        }
        self.players[a].alliances.iter().copied().find(|&al| {
            let x = &self.alliances[al];
            x.requestor == b || x.recipient == b
        })
    }

    pub(super) fn can_send_alliance_request(&self, a: usize, other: usize) -> bool {
        if self.config.disable_alliances || other == a {
            return false;
        }
        if self.players[a].is_disconnected || self.players[other].is_disconnected {
            return false;
        }
        if self.is_friendly(a, other, false) || !self.players[a].is_alive() {
            return false;
        }
        if self.outgoing_alliance_requests(a).iter().any(|&r| self.requests[r].recipient == other) {
            return false;
        }
        if self.incoming_alliance_requests(a).iter().any(|&r| self.requests[r].requestor == other) {
            return true;
        }
        let latest = self.players[a]
            .past_outgoing_requests
            .iter()
            .filter(|&&r| self.requests[r].recipient == other)
            .map(|&r| self.requests[r].created_at)
            .max();
        match latest {
            None => true,
            Some(created) => self.ticks - created >= 300,
        }
    }

    /// GameImpl.createAllianceRequest
    pub(super) fn create_alliance_request(&mut self, requestor: usize, recipient: usize) -> Option<usize> {
        assert!(!self.is_allied_with(requestor, recipient), "cannot create alliance request, already allies");
        if self.incoming_alliance_requests(recipient).iter().any(|&r| self.requests[r].requestor == requestor) {
            return None;
        }
        if let Some(corresponding) =
            self.incoming_alliance_requests(requestor).into_iter().find(|&r| self.requests[r].requestor == recipient)
        {
            self.accept_alliance_request(corresponding);
            return None;
        }
        let idx = self.requests.len();
        self.requests.push(AllianceRequest { requestor, recipient, created_at: self.ticks, status: RequestStatus::Pending });
        self.alliance_requests.push(idx);
        Some(idx)
    }

    pub(super) fn accept_alliance_request(&mut self, r: usize) {
        self.requests[r].status = RequestStatus::Accepted;
        self.alliance_requests.retain(|&x| x != r);
        let (requestor, recipient) = (self.requests[r].requestor, self.requests[r].recipient);
        assert!(self.alliance_with(requestor, recipient).is_none(), "cannot accept alliance request, already allied");
        let id = self.next_alliance_id;
        self.next_alliance_id += 1;
        let idx = self.alliances.len();
        self.alliances.push(Alliance {
            id,
            requestor,
            recipient,
            created_at: self.ticks,
            expires_at: self.ticks + self.config.alliance_duration,
            ext_requestor: false,
            ext_recipient: false,
        });
        self.players[requestor].alliances.push(idx);
        self.players[recipient].alliances.push(idx);
        self.players[requestor].past_outgoing_requests.push(r);
    }

    pub(super) fn reject_alliance_request(&mut self, r: usize) {
        self.requests[r].status = RequestStatus::Rejected;
        self.alliance_requests.retain(|&x| x != r);
        let requestor = self.requests[r].requestor;
        self.players[requestor].past_outgoing_requests.push(r);
    }

    pub(super) fn detach_alliance(&mut self, al: usize) {
        let (a, b) = (self.alliances[al].requestor, self.alliances[al].recipient);
        self.players[a].alliances.retain(|&x| x != al);
        self.players[b].alliances.retain(|&x| x != al);
    }

    pub(super) fn expire_alliance(&mut self, al: usize) {
        self.detach_alliance(al);
    }

    pub(super) fn remove_all_alliances(&mut self, p: usize) {
        for al in self.players[p].alliances.clone() {
            self.detach_alliance(al);
        }
    }

    pub(super) fn break_alliance(&mut self, breaker: usize, al: usize) {
        let other = self.alliances[al].other(breaker);
        assert!(self.is_allied_with(breaker, other), "not allied, cannot break alliance");
        if !self.is_traitor(other) && !self.players[other].is_disconnected {
            self.players[breaker].marked_traitor_tick = self.ticks as i64;
        }
        self.detach_alliance(al);
    }

    /// AttackExecution / TransportShipExecution.rejectIncomingAllianceRequests
    pub(super) fn reject_incoming_alliance_requests(&mut self, owner: usize, target: usize) {
        if let Some(r) = self.incoming_alliance_requests(owner).into_iter().find(|&r| self.requests[r].requestor == target) {
            self.reject_alliance_request(r);
        }
    }

    pub(super) fn end_temporary_embargo(&mut self, p: usize, other: usize) {
        if let Some(e) = self.players[p].embargoes.iter().find(|e| e.target == other) {
            if !e.is_temporary {
                return;
            }
        }
        self.players[p].embargoes.retain(|e| e.target != other);
    }

    pub(super) fn has_embargo_against(&self, p: usize, other: usize) -> bool {
        self.players[p].embargoes.iter().any(|e| e.target == other)
    }

    pub(super) fn init_alliance_request(&mut self, ae: &mut AllianceRequestExec) {
        let Some(recipient) = self.player_index(&ae.recipient_id) else { return };
        if !self.can_send_alliance_request(ae.requestor, recipient) {
            ae.active = false;
            return;
        }
        let requestor = ae.requestor;
        if let Some(incoming) =
            self.outgoing_alliance_requests(recipient).into_iter().find(|&r| self.requests[r].recipient == requestor)
        {
            ae.active = false;
            self.accept_alliance_request(incoming);
            self.update_relation(requestor, recipient, 100.0);
            self.update_relation(recipient, requestor, 100.0);
            if self.has_embargo_against(requestor, recipient) {
                self.end_temporary_embargo(requestor, recipient);
            }
            if self.has_embargo_against(recipient, requestor) {
                self.end_temporary_embargo(recipient, requestor);
            }
            self.cancel_nukes_between_allied(requestor, recipient);
        } else {
            ae.req = self.create_alliance_request(requestor, recipient);
        }
    }

    /// AllianceRequestExecution.cancelNukesBetweenAlliedPlayers
    pub(super) fn cancel_nukes_between_allied(&mut self, requestor: usize, recipient: usize) {
        for launcher in [requestor, recipient] {
            let other = if launcher == requestor { recipient } else { requestor };
            let nukes: Vec<usize> = self.players[launcher]
                .units
                .iter()
                .copied()
                .filter(|&u| matches!(self.units[u].type_name, ATOM_BOMB | HYDROGEN_BOMB | "MIRV" | MIRV_WARHEAD))
                .collect();
            for u in nukes {
                if !self.units[u].active {
                    continue;
                }
                let target_tile = self.units[u].target_tile;
                let cancel = match self.units[u].type_name {
                    "MIRV" => self.units[u].mirv_target.or_else(|| target_tile.and_then(|t| self.owner_index(t))) == Some(other),
                    MIRV_WARHEAD => target_tile.is_some_and(|t| self.owner_index(t) == Some(other)),
                    _ => target_tile.is_some_and(|t| self.would_nuke_break_alliance(t, self.units[u].type_name, other)),
                };
                if cancel {
                    self.delete_unit(u);
                }
            }
        }
    }

    /// wouldNukeBreakAlliance with a single ally.
    pub(super) fn would_nuke_break_alliance(&self, target: u32, nuke_type: &str, ally: usize) -> bool {
        let (inner, outer) = Self::nuke_magnitude(nuke_type);
        let structures = [CITY, DEFENSE_POST, SAM_LAUNCHER, MISSILE_SILO, PORT, FACTORY];
        if self.any_nearby_unit(target, outer, &structures, |u, _| self.units[u].owner == ally) {
            return true;
        }
        let (inner2, outer2) = (inner * inner, outer * outer);
        let ally_sid = self.players[ally].small_id;
        let (cx, cy) = (self.map.x(target) as i64, self.map.y(target) as i64);
        let (w, h) = (self.map.width as i64, self.map.height as i64);
        let mut count = 0.0;
        for x in (cx - outer).max(0)..=(cx + outer).min(w - 1) {
            for y in (cy - outer).max(0)..=(cy + outer).min(h - 1) {
                let d2 = (x - cx) * (x - cx) + (y - cy) * (y - cy);
                if d2 <= outer2 && self.map.owner_id((y * w + x) as u32) == ally_sid {
                    count += if d2 <= inner2 { 1.0 } else { 0.5 };
                    if count > 100.0 {
                        return true;
                    }
                }
            }
        }
        false
    }

    pub(super) fn tick_alliance_request(&mut self, ae: &mut AllianceRequestExec) {
        if let Some(r) = ae.req {
            if self.requests[r].status != RequestStatus::Pending {
                ae.active = false;
                return;
            }
        }
        let created = ae.req.map(|r| self.requests[r].created_at).unwrap_or(0);
        if self.ticks as i64 - created as i64 > 200 {
            if let Some(r) = ae.req {
                self.reject_alliance_request(r);
            }
            ae.active = false;
        }
    }

    pub(super) fn init_alliance_reject(&mut self, requestor_id: &str, recipient: usize) {
        let Some(requestor) = self.player_index(requestor_id) else { return };
        if self.is_friendly(requestor, recipient, false) {
            return;
        }
        if let Some(r) = self.outgoing_alliance_requests(requestor).into_iter().find(|&r| self.requests[r].recipient == recipient) {
            self.reject_alliance_request(r);
        }
    }

    pub(super) fn tick_break_alliance(&mut self, requestor: usize, recipient: usize) {
        let Some(al) = self.alliance_with(requestor, recipient) else { return };
        self.break_alliance(requestor, al);
        self.update_relation(recipient, requestor, -100.0);
        let neighbors: Vec<usize> = self
            .nearby(requestor)
            .into_iter()
            .flatten()
            .filter(|&n| !self.is_on_same_team(n, recipient))
            .collect();
        for n in neighbors {
            self.update_relation(n, requestor, -40.0);
        }
    }

    /// TribeExecution.acceptAllAllianceRequests
    pub(super) fn tribe_accept_alliance_requests(&mut self, p: usize) {
        for r in self.incoming_alliance_requests(p) {
            self.accept_alliance_request(r);
        }
        for al in self.players[p].alliances.clone() {
            let a = &self.alliances[al];
            if a.ext_requestor != a.ext_recipient {
                let other = a.other(p);
                let to_id = self.players[other].info.id.clone();
                self.add_execution(Exec::AllianceExtension { from: p, to_id });
            }
        }
    }
}


// ── Donations (DonateTroopsExecution, DonateGoldExecution) ──

pub struct DonateExec {
    pub(super) active: bool,
    pub(super) gold: bool,
    pub(super) sender: usize,
    pub(super) recipient_id: String,
    pub(super) recipient: Option<usize>,
    pub(super) amount: Option<f64>,
    pub(super) random: Option<PseudoRandom>,
}

impl Game {
    pub fn new_donation(&self, sender: usize, gold: bool, recipient_id: String, amount: Option<f64>) -> Exec {
        Exec::Donate(Box::new(DonateExec {
            active: true,
            gold,
            sender,
            recipient_id,
            recipient: None,
            // DonateGoldExecution: toInt in the constructor.
            amount: if gold { amount.map(|a| a.floor()) } else { amount },
            random: None,
        }))
    }

    pub(super) fn init_donation(&mut self, de: &mut DonateExec) {
        de.random = Some(PseudoRandom::new(self.ticks as f64));
        let Some(r) = self.player_index(&de.recipient_id) else {
            de.active = false;
            return;
        };
        de.recipient = Some(r);
        if de.gold {
            // sender.gold() / 3n
            de.amount = Some(de.amount.unwrap_or((self.players[de.sender].gold / 3) as f64));
            return;
        }
        let troops = de.amount.unwrap_or((self.players[de.sender].troops_f() / 3.0).floor());
        let max_donation = self.max_troops(r) - self.players[r].troops_f();
        let troops = js_min(troops, max_donation);
        de.amount = Some(troops);
        if troops <= 0.0 {
            de.active = false;
        }
    }

    /// canDonateTroops / canDonateGold
    pub(super) fn can_donate(&self, sender: usize, recipient: usize, gold: bool) -> bool {
        if recipient == sender {
            return false;
        }
        if !self.players[sender].is_alive() || !self.players[recipient].is_alive() || !self.is_friendly(sender, recipient, false) {
            return false;
        }
        let allowed = if gold { self.config.donate_gold } else { self.config.donate_troops };
        if self.players[recipient].info.player_type == PlayerType::Human && !allowed {
            return false;
        }
        !self.players[sender]
            .sent_donations
            .iter()
            .any(|&(r, t)| r == recipient && (self.ticks as i64 - t as i64) < 100)
    }

    /// Only nations answer a donation, with an emoji picked from `list`.
    pub(super) fn donation_emoji_reply(&mut self, de: &mut DonateExec, list: &[&str]) {
        let (sender, recipient) = (de.sender, de.recipient.expect("initialized"));
        if self.players[recipient].info.player_type != PlayerType::Nation || !self.can_send_emoji(recipient, Some(sender)) {
            return;
        }
        let ids = emoji_ids(list);
        let emoji = ids[de.random.as_mut().unwrap().next_int(0.0, ids.len() as f64) as usize];
        let to = self.players[sender].info.id.clone();
        self.add_execution(Exec::Emoji(Box::new(EmojiExec::new(recipient, to, emoji))));
    }

    pub(super) fn tick_donation(&mut self, de: &mut DonateExec) {
        let (sender, recipient) = (de.sender, de.recipient.expect("initialized"));
        let amount = de.amount.expect("not initialized");
        if de.gold {
            if self.can_donate(sender, recipient, true) && amount > 0.0 {
                let removed = self.remove_gold(sender, amount as i64);
                if removed != 0 {
                    self.add_gold(recipient, removed);
                    self.players[sender].sent_donations.push((recipient, self.ticks));
                    // calculateRelationUpdate (getGoldChunkSize)
                    let chunk = self.by_difficulty([2_500.0, 5_000.0, 12_500.0, 25_000.0]);
                    let mult = self.ticks as f64 / (3000.0 + self.config.num_spawn_phase_turns() as f64);
                    let adjusted = (chunk + chunk * mult + 0.5).floor() as i64;
                    let chunks = amount as i64 / adjusted;
                    let update = (chunks * 5).min(100);
                    if update > 0 {
                        self.update_relation(recipient, sender, update as f64);
                    }
                    let list: &[&str] = if update >= 50 { &["❤️", "😊", "🥰"] } else if update > 0 { &["👍"] } else { &["❓", "🥱"] };
                    self.donation_emoji_reply(de, list);
                }
            }
        } else {
            // getMinTroopsForRelationUpdate
            let max = self.max_troops(recipient);
            let (lo, hi) = self.by_difficulty([(13.0, 11.0), (11.0, 9.0), (9.0, 7.0), (7.0, 5.0)]);
            let min_troops = de.random.as_mut().unwrap().next_int(max / lo, max / hi);
            if self.can_donate(sender, recipient, false) && amount > 0.0 {
                let removed = self.remove_troops(sender, amount);
                if removed != 0.0 {
                    self.add_troops(recipient, removed);
                    self.players[sender].sent_donations.push((recipient, self.ticks));
                    if amount >= min_troops {
                        self.update_relation(recipient, sender, 50.0);
                    }
                    let list: &[&str] = if amount >= min_troops { &["❤️", "😊", "🥰"] } else { &["❓", "🥱"] };
                    self.donation_emoji_reply(de, list);
                }
            }
        }
        de.active = false;
    }
}

