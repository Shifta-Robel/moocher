//! Request rates (`quota.rate`, `quota.models.<m>.rate`).
//!
//! A one-second limit is too short to park a session for, so a call above the
//! rate waits here, the most urgent first. Counts are this node's own: several
//! nodes on one key each get the whole rate, and the provider's 429s catch the
//! rest.

use crate::quota::Target;
use moocher_protocol::config::RateLimit;
use moocher_protocol::model::{now_ms, Ts};
use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Mutex;

/// A wait up to this long keeps a session on its key (and its prompt cache).
pub const STAY_MS: i64 = 2_000;
/// After this the call goes back to OpenCode as rate limited, which retries.
pub const MAX_WAIT_MS: i64 = 90_000;
const POLL_MS: i64 = 100;

#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    id: String,
    max: u32,
    window_ms: i64,
}

fn push(out: &mut Vec<Rule>, id: &str, r: &RateLimit) {
    if let Some(max) = r.rps {
        out.push(Rule { id: format!("{id} 1s"), max, window_ms: 1000 });
    }
    if let Some(max) = r.rpm {
        out.push(Rule { id: format!("{id} 1m"), max, window_ms: 60_000 });
    }
}

/// What a call to `model` through `t` has to fit.
pub fn rules(provider: &str, t: &Target, shared: Option<&RateLimit>, own: Option<&RateLimit>, model: &str) -> Vec<Rule> {
    let mut out = vec![];
    if let Some(r) = shared {
        push(&mut out, &format!("{provider} {}", t.scope_id), r);
    }
    if let Some(r) = own {
        push(&mut out, &format!("{provider} {}", t.for_model(model).scope_id), r);
    }
    out
}

type Ticket = (Reverse<u8>, u64);

#[derive(Default)]
struct Inner {
    sent: HashMap<String, VecDeque<Ts>>,
    waiting: BTreeMap<Ticket, Vec<Rule>>,
    seq: u64,
}

impl Inner {
    /// When this rule has room next.
    fn room_at(&mut self, r: &Rule, now: Ts) -> Ts {
        let Some(q) = self.sent.get_mut(&r.id) else { return now };
        while q.front().is_some_and(|t| now - t >= r.window_ms) {
            q.pop_front();
        }
        if (q.len() as u32) < r.max {
            now
        } else {
            q.front().map_or(now, |t| t + r.window_ms)
        }
    }

    /// Let through whoever fits, the most urgent first. Someone waiting on one
    /// rule does not hold up others that fit theirs.
    fn grant(&mut self, now: Ts) -> Vec<Ticket> {
        let mut out = vec![];
        for (ticket, rules) in std::mem::take(&mut self.waiting) {
            if rules.iter().all(|r| self.room_at(r, now) <= now) {
                for r in &rules {
                    self.sent.entry(r.id.clone()).or_default().push_back(now);
                }
                out.push(ticket);
            } else {
                self.waiting.insert(ticket, rules);
            }
        }
        out
    }
}

#[derive(Default)]
pub struct Pacer {
    inner: Mutex<Inner>,
    granted: Mutex<std::collections::HashSet<u64>>,
}

/// Leaves the queue if the call is dropped while it waits.
struct Waiting<'a> {
    pacer: &'a Pacer,
    ticket: Ticket,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.pacer.inner.lock().unwrap().waiting.remove(&self.ticket);
        self.pacer.granted.lock().unwrap().remove(&self.ticket.1);
    }
}

impl Pacer {
    /// How long a call would wait now, others in the queue included.
    pub fn wait_ms(&self, rules: &[Rule], now: Ts) -> i64 {
        let mut m = self.inner.lock().unwrap();
        let mut wait = 0;
        for r in rules {
            let ahead = m.waiting.values().filter(|w| w.iter().any(|x| x.id == r.id)).count() as i64;
            let own = m.room_at(r, now) - now;
            wait = wait.max(own + ahead * r.window_ms / r.max.max(1) as i64);
        }
        wait
    }

    /// Wait for room and take it. False when `max_wait_ms` passed first.
    pub async fn acquire(&self, rules: &[Rule], level: u8, max_wait_ms: i64) -> bool {
        if rules.is_empty() {
            return true;
        }
        let deadline = now_ms() + max_wait_ms;
        let ticket = {
            let mut m = self.inner.lock().unwrap();
            m.seq += 1;
            let ticket = (Reverse(level), m.seq);
            m.waiting.insert(ticket, rules.to_vec());
            ticket
        };
        let _waiting = Waiting { pacer: self, ticket };
        loop {
            let now = now_ms();
            let next = {
                let mut m = self.inner.lock().unwrap();
                let done = m.grant(now);
                let mut g = self.granted.lock().unwrap();
                g.extend(done.iter().map(|t| t.1));
                if g.remove(&ticket.1) {
                    return true;
                }
                rules.iter().map(|r| m.room_at(r, now)).max().unwrap_or(now)
            };
            if now >= deadline {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis((next - now).clamp(10, POLL_MS) as u64)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(id: &str, max: u32, window_ms: i64) -> Rule {
        Rule { id: id.into(), max, window_ms }
    }

    #[test]
    fn the_most_urgent_goes_first_and_a_stuck_one_holds_nobody_up() {
        let mut m = Inner::default();
        let (shared, slow) = (rule("key 1s", 2, 1000), rule("model 1m", 1, 60_000));
        m.sent.insert(slow.id.clone(), [1000].into());
        m.waiting.insert((Reverse(1), 1), vec![shared.clone(), slow.clone()]);
        m.waiting.insert((Reverse(1), 2), vec![shared.clone()]);
        m.waiting.insert((Reverse(3), 3), vec![shared.clone()]);
        // right_now first, then the normal call that fits; the one over its model's rate waits.
        assert_eq!(m.grant(1000), vec![(Reverse(3), 3), (Reverse(1), 2)]);
        assert_eq!(m.waiting.len(), 1);
        // Later the model's window is over.
        assert_eq!(m.grant(61_000), vec![(Reverse(1), 1)]);
    }

    #[tokio::test]
    async fn calls_above_the_rate_wait_and_give_up_in_time() {
        let p = Pacer::default();
        let r = [rule("k 1s", 2, 300)];
        let start = now_ms();
        for _ in 0..3 {
            assert!(p.acquire(&r, 1, 2000).await);
        }
        assert!(now_ms() - start >= 280);
        let tight = [rule("t", 1, 60_000)];
        assert!(p.acquire(&tight, 1, 50).await);
        assert!(!p.acquire(&tight, 1, 50).await);
        assert!(p.inner.lock().unwrap().waiting.is_empty());
    }
}
