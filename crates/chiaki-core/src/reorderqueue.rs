// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// Port von lib/src/reorderqueue.c + lib/include/chiaki/reorderqueue.h (chiaki-ng).
//
// Reorder-Queue mit 2^size_exp Ring-Einträgen, Fenster [begin, begin+count)
// und Serial-Number-Arithmetik (SeqNum16/32 — SeqNumSize ersetzt hier die
// C-Funktionszeiger seq_num_gt/lt/add aus dem Header).
//
// Abweichung mit Relevanz: chiaki_reorder_queue_drop() in der chiaki-ng-
// Referenz (lib/src/reorderqueue.c) löscht `entry->set` NICHT — die ursprüngliche
// chiaki-Implementierung setzte es auf false. In Rust erhält der Callback das
// Element by value, der Slot MUSS geleert werden (Invariante:
// set == user.is_some()); das beobachtbare Verhalten entspricht dem chiaki-Original.

use crate::error::{ChiakiError, ChiakiResult};
use crate::seqnum;

/// Port von `ChiakiReorderQueueDropStrategy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropStrategy {
    /// drop packet with lowest number
    Begin,
    /// drop packet with highest number (Default wie im C-Init)
    End,
}

/// Ersetzt die C-Funktionszeiger (`ChiakiReorderQueueSeqNumGt/Lt/Add`):
/// Breite der Sequenznummern-Arithmetik.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeqNumSize {
    Num16,
    Num32,
}

struct Entry<T> {
    set: bool,
    user: Option<T>,
}

/// Port von `ChiakiReorderQueue`.
pub struct ReorderQueue<T> {
    size_exp: usize, // real size = 2^size_exp Einträge
    queue: Vec<Entry<T>>,
    begin: u64,
    count: u64,
    kind: SeqNumSize,
    drop_strategy: DropStrategy,
    drop_cb: Option<Box<dyn FnMut(u64, T) + Send>>,
}

impl<T> ReorderQueue<T> {
    /// Port von `chiaki_reorder_queue_init()`.
    ///
    /// @param size_exp Exponent für 2 (Ringgröße = 2^size_exp)
    /// @param seq_num_start Sequenznummer des ersten erwarteten Elements
    pub fn new(size_exp: usize, seq_num_start: u64, kind: SeqNumSize) -> ChiakiResult<Self> {
        if size_exp >= usize::BITS as usize {
            return Err(ChiakiError::Overflow);
        }
        Ok(ReorderQueue {
            size_exp,
            // calloc-Äquivalent: alle Slots unset/leer
            queue: (0..(1usize << size_exp))
                .map(|_| Entry { set: false, user: None })
                .collect(),
            begin: seq_num_start,
            count: 0,
            kind,
            drop_strategy: DropStrategy::End,
            drop_cb: None,
        })
    }

    /// Port von `chiaki_reorder_queue_set_drop_strategy()`.
    pub fn set_drop_strategy(&mut self, s: DropStrategy) {
        self.drop_strategy = s;
    }

    /// Port von `chiaki_reorder_queue_set_drop_cb()`.
    ///
    /// Der Callback wird mit (seq_num, element) für jedes verworfene Element
    /// aufgerufen — auch für das neue Element, wenn bereits eines mit derselben
    /// Sequenznummer existiert oder die Nummer < begin ist.
    pub fn set_drop_cb(&mut self, cb: Option<Box<dyn FnMut(u64, T) + Send>>) {
        self.drop_cb = cb;
    }

    /// Port von `chiaki_reorder_queue_size()` (2^size_exp).
    pub fn size(&self) -> u64 {
        1u64 << self.size_exp
    }

    /// Port von `chiaki_reorder_queue_count()`.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Beginn des Fensters (C: direkter Zugriff auf queue->begin).
    pub fn begin(&self) -> u64 {
        self.begin
    }

    /// Serial-Number-gt der konfigurierten Breite (C-Makro `gt`).
    pub fn seq_num_gt(&self, a: u64, b: u64) -> bool {
        match self.kind {
            SeqNumSize::Num16 => seqnum::seq_num_16_gt(a as u16, b as u16),
            SeqNumSize::Num32 => seqnum::seq_num_32_gt(a as u32, b as u32),
        }
    }

    /// Serial-Number-lt der konfigurierten Breite (C-Makro `lt`).
    pub fn seq_num_lt(&self, a: u64, b: u64) -> bool {
        match self.kind {
            SeqNumSize::Num16 => seqnum::seq_num_16_lt(a as u16, b as u16),
            SeqNumSize::Num32 => seqnum::seq_num_32_lt(a as u32, b as u32),
        }
    }

    /// Wrapping-Addition in der konfigurierten Breite (C-Makro `add`).
    pub fn seq_num_add(&self, a: u64, b: u64) -> u64 {
        match self.kind {
            SeqNumSize::Num16 => (a as u16).wrapping_add(b as u16) as u64,
            SeqNumSize::Num32 => (a as u32).wrapping_add(b as u32) as u64,
        }
    }

    fn ge(&self, a: u64, b: u64) -> bool {
        a == b || self.seq_num_gt(a, b)
    }

    fn lt(&self, a: u64, b: u64) -> bool {
        self.seq_num_lt(a, b)
    }

    fn idx(&self, seq_num: u64) -> usize {
        (seq_num & (self.size() - 1)) as usize
    }

    fn invoke_drop(&mut self, seq_num: u64, user: T) {
        if let Some(mut cb) = self.drop_cb.take() {
            cb(seq_num, user);
            self.drop_cb = Some(cb);
        }
    }

    /// Port von `chiaki_reorder_queue_push()`.
    ///
    /// Je nach Drop-Strategie können Elemente verworfen werden (Callback);
    /// der Callback wird auch für das neue Element gerufen, wenn bereits ein
    /// Element mit derselben Sequenznummer existiert oder seq_num < begin.
    pub fn push(&mut self, seq_num: u64, user: T) {
        assert!(self.count <= self.size());
        let mut end = self.seq_num_add(self.begin, self.count);

        if self.ge(seq_num, self.begin) && self.lt(seq_num, end) {
            let idx = self.idx(seq_num);
            let entry = &mut self.queue[idx];
            if entry.set {
                // received twice
                self.invoke_drop(seq_num, user);
                return;
            }
            entry.user = Some(user);
            entry.set = true;
            return;
        }

        if self.lt(seq_num, self.begin) {
            self.invoke_drop(seq_num, user);
            return;
        }

        // => ge(seq_num, queue->end) == 1
        if !self.ge(seq_num, end) {
            // Sequence comparisons are undefined at half the serial-number space.
            // If the queue is empty and callers opted into dropping from the begin,
            // rebase to the new packet; otherwise drop it rather than aborting.
            if self.count == 0 && self.drop_strategy == DropStrategy::Begin {
                self.begin = seq_num;
                end = seq_num;
            } else {
                self.invoke_drop(seq_num, user);
                return;
            }
        }

        let mut free_elems = self.size() - self.count;
        let mut total_end = self.seq_num_add(end, free_elems);
        let new_end = self.seq_num_add(seq_num, 1);
        if self.lt(total_end, new_end) {
            if self.drop_strategy == DropStrategy::End {
                self.invoke_drop(seq_num, user);
                return;
            }

            // drop first until empty or enough space
            while self.count > 0 && self.lt(total_end, new_end) {
                let head = self.begin;
                let head_idx = self.idx(head);
                let dropped = {
                    let entry = &mut self.queue[head_idx];
                    entry.set = false;
                    entry.user.take()
                };
                if let Some(u) = dropped {
                    self.invoke_drop(head, u);
                }
                self.begin = self.seq_num_add(self.begin, 1);
                self.count -= 1;
                free_elems = self.size() - self.count;
                total_end = self.seq_num_add(end, free_elems);
            }

            // empty, just shift to the seq_num
            if self.count == 0 {
                self.begin = seq_num;
            }
        }

        // move end until new_end
        end = self.seq_num_add(self.begin, self.count);
        while self.lt(end, new_end) {
            self.count += 1;
            let end_idx = self.idx(end);
            let entry = &mut self.queue[end_idx];
            entry.set = false;
            entry.user = None;
            end = self.seq_num_add(self.begin, self.count);
            assert!(self.count <= self.size());
        }

        let seq_idx = self.idx(seq_num);
        let entry = &mut self.queue[seq_idx];
        entry.set = true;
        entry.user = Some(user);
    }

    /// Port von `chiaki_reorder_queue_pull()`: nächstes Element in-order.
    ///
    /// Wiederholt aufrufen, bis `None`, um alle verfügbaren Elemente zu holen.
    pub fn pull(&mut self) -> Option<(u64, T)> {
        assert!(self.count <= self.size());
        if self.count == 0 {
            return None;
        }

        let begin_idx = self.idx(self.begin);
        let entry = &mut self.queue[begin_idx];
        if !entry.set {
            return None;
        }
        let seq_num = self.begin;
        let user = entry.user.take();
        // C lässt `set` hier stehen; der Slot liegt damit außerhalb des
        // Fensters und wird bei Wiederverwertung eh zurückgesetzt. Rust hält
        // die Invariante set == user.is_some() strikt durch.
        entry.set = false;
        self.begin = self.seq_num_add(self.begin, 1);
        self.count -= 1;
        Some((seq_num, user?))
    }

    /// Port von `chiaki_reorder_queue_peek()`.
    ///
    /// @param index Offset auf begin (KEINE Sequenznummer!), 0 <= index < count
    pub fn peek(&self, index: u64) -> Option<(u64, &T)> {
        if index >= self.count {
            return None;
        }
        let seq_num = self.seq_num_add(self.begin, index);
        let entry = &self.queue[self.idx(seq_num)];
        if !entry.set {
            return None;
        }
        entry.user.as_ref().map(|u| (seq_num, u))
    }

    /// Mutable Variante von [`peek`](Self::peek) für In-place-Verarbeitung
    /// (z. B. MAC-Re-Check der queued Data-Pakete in takion.rs).
    pub fn peek_mut(&mut self, index: u64) -> Option<(u64, &mut T)> {
        if index >= self.count {
            return None;
        }
        let seq_num = self.seq_num_add(self.begin, index);
        let entry_idx = self.idx(seq_num);
        let entry = &mut self.queue[entry_idx];
        if !entry.set {
            return None;
        }
        entry.user.as_mut().map(|u| (seq_num, u))
    }

    /// Port von `chiaki_reorder_queue_drop()`. begin wird nicht geändert.
    ///
    /// @param index Offset auf begin (KEINE Sequenznummer!), 0 <= index < count
    pub fn drop_at(&mut self, index: u64) {
        if index >= self.count {
            return;
        }

        let mut seq_num = self.seq_num_add(self.begin, index);
        let (was_set, user) = {
            let entry_idx = self.idx(seq_num);
            let entry = &mut self.queue[entry_idx];
            (entry.set, entry.user.take())
        };
        if !was_set {
            return;
        }
        if let Some(u) = user {
            self.invoke_drop(seq_num, u);
        }

        // reduce count if necessary
        if index == self.count - 1 {
            // Das gedroppte Element war das letzte im Fenster: count so weit
            // reduzieren, bis hinten wieder ein gesetztes Element liegt.
            self.count -= 1;
            while self.count > 0 {
                seq_num = self.seq_num_add(self.begin, self.count - 1);
                if self.queue[self.idx(seq_num)].set {
                    break;
                }
                self.count -= 1;
            }
        }
    }

    /// C: direktes `queue->begin = add(begin, skipped); queue->count -= skipped;`
    /// aus takion_av_queue_flush_with_timeout() — Überspringen von Lücken am
    /// Kopf nach Ablauf des Reorder-Timeouts (ohne Drop-Callback, wie im C).
    pub fn skip_head(&mut self, skipped: u64) {
        assert!(skipped <= self.count);
        self.begin = self.seq_num_add(self.begin, skipped);
        self.count -= skipped;
    }
}

impl<T> Drop for ReorderQueue<T> {
    /// Port von `chiaki_reorder_queue_fini()`: verbleibende Elemente an den
    /// Drop-Callback übergeben.
    fn drop(&mut self) {
        if let Some(mut cb) = self.drop_cb.take() {
            for i in 0..self.count {
                let seq_num = self.seq_num_add(self.begin, i);
                let entry_idx = self.idx(seq_num);
                let entry = &mut self.queue[entry_idx];
                if entry.set {
                    entry.set = false;
                    if let Some(u) = entry.user.take() {
                        cb(seq_num, u);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    // Golden-Test aus chiaki-ng test/reorderqueue.c (test_reorder_queue_16),
    // 1:1 übernommen. Elemente sind u64-Werte wie die C-Pointer-Casts.

    const DROP_RECORD_MAX: usize = 16;

    #[derive(Default)]
    struct DropRecord {
        count: [u64; DROP_RECORD_MAX],
        seq_num: [u64; DROP_RECORD_MAX],
        failed: bool,
    }

    impl DropRecord {
        fn drop_cb(&mut self, seq_num: u64, elem: u64) {
            let v = elem as usize;
            if v > DROP_RECORD_MAX {
                self.failed = true;
                return;
            }
            self.count[v] += 1;
            self.seq_num[v] = seq_num;
        }
    }

    #[test]
    fn test_reorder_queue_16() {
        // Vor der Queue deklariert: das Drop-Callback leiht `record`, und die
        // Queue (mit dem Callback) droppt nach `record`.
        let record = Arc::new(Mutex::new(DropRecord::default()));
        let mut queue = ReorderQueue::new(2, 42, SeqNumSize::Num16).unwrap();
        assert_eq!(queue.size(), 4);
        assert_eq!(queue.count(), 0);

        queue.set_drop_strategy(DropStrategy::End);
        let record_cb = Arc::clone(&record);
        queue.set_drop_cb(Some(Box::new(move |seq_num, elem: u64| {
            record_cb.lock().unwrap().drop_cb(seq_num, elem)
        })));

        // pull from empty
        assert!(queue.pull().is_none());
        assert_eq!(queue.count(), 0);
        assert!(!record.lock().unwrap().failed);

        // push one
        queue.push(42, 0);
        assert_eq!(queue.count(), 1);

        // pull one
        let (seq_num, user) = queue.pull().expect("pull after push");
        assert_eq!(queue.count(), 0);
        assert!(!record.lock().unwrap().failed);
        assert_eq!(record.lock().unwrap().count[0], 0);
        assert_eq!(user, 0);
        assert_eq!(seq_num, 42);

        // push outdated
        queue.push(42, 0);
        assert_eq!(queue.count(), 0);
        assert!(!record.lock().unwrap().failed);
        assert_eq!(record.lock().unwrap().count[0], 1);
        assert_eq!(record.lock().unwrap().seq_num[0], 42);
        *record.lock().unwrap() = DropRecord::default();

        // push until full out of order and try to pull in between
        queue.push(46, 1);
        assert!(queue.pull().is_none());
        queue.push(45, 2);
        assert!(queue.pull().is_none());
        queue.push(44, 3);
        assert!(queue.pull().is_none());
        queue.push(43, 4);
        assert!(!record.lock().unwrap().failed);
        for c in record.lock().unwrap().count.iter() {
            assert_eq!(*c, 0);
        }

        // push more, because of DROP_STRATEGY_END this should be dropped
        queue.push(47, 5);
        assert!(!record.lock().unwrap().failed);
        for (i, c) in record.lock().unwrap().count.iter().enumerate() {
            assert_eq!(*c, if i == 5 { 1 } else { 0 });
        }
        assert_eq!(record.lock().unwrap().seq_num[5], 47);
        *record.lock().unwrap() = DropRecord::default();

        // push more with DROP_STRATEGY_BEGIN, so older elements should be dropped
        queue.set_drop_strategy(DropStrategy::Begin);
        queue.push(47, 5);
        assert!(!record.lock().unwrap().failed);
        for (i, c) in record.lock().unwrap().count.iter().enumerate() {
            assert_eq!(*c, if i == 4 { 1 } else { 0 });
        }
        assert_eq!(record.lock().unwrap().seq_num[4], 43);
        *record.lock().unwrap() = DropRecord::default();

        // pull all, elements should arrive in order
        let (seq_num, user) = queue.pull().expect("pull 44");
        assert_eq!(seq_num, 44);
        assert_eq!(user, 3);

        let (seq_num, user) = queue.pull().expect("pull 45");
        assert_eq!(seq_num, 45);
        assert_eq!(user, 2);

        let (seq_num, user) = queue.pull().expect("pull 46");
        assert_eq!(seq_num, 46);
        assert_eq!(user, 1);

        let (seq_num, user) = queue.pull().expect("pull 47");
        assert_eq!(seq_num, 47);
        assert_eq!(user, 5);

        // should be empty now again
        assert!(queue.pull().is_none());
        assert_eq!(queue.count(), 0);

        assert!(!record.lock().unwrap().failed);
        for c in record.lock().unwrap().count.iter() {
            assert_eq!(*c, 0);
        }

        // now push something much higher, because of DROP_STRATEGY_BEGIN,
        // the queue should be relocated
        queue.push(1337, 6);
        assert_eq!(queue.count(), 1);
        assert!(!record.lock().unwrap().failed);
        for c in record.lock().unwrap().count.iter() {
            assert_eq!(*c, 0);
        }

        // and pull again
        let (seq_num, user) = queue.pull().expect("pull 1337");
        assert_eq!(seq_num, 1337);
        assert_eq!(user, 6);
        assert_eq!(queue.count(), 0);

        // same as before, but with an element in the queue that will be dropped
        queue.push(1338, 7);
        assert_eq!(queue.count(), 1);
        assert!(!record.lock().unwrap().failed);
        for c in record.lock().unwrap().count.iter() {
            assert_eq!(*c, 0);
        }

        queue.push(2000, 8);
        assert_eq!(queue.count(), 1);
        assert!(!record.lock().unwrap().failed);
        for (i, c) in record.lock().unwrap().count.iter().enumerate() {
            assert_eq!(*c, if i == 7 { 1 } else { 0 });
        }
        assert_eq!(record.lock().unwrap().seq_num[7], 1338);

        // pull again
        let (seq_num, user) = queue.pull().expect("pull 2000");
        assert_eq!(seq_num, 2000);
        assert_eq!(user, 8);
        assert_eq!(queue.count(), 0);

        drop(queue); // fini ohne verbleibende Elemente
        assert!(!record.lock().unwrap().failed);
    }

    // Ergänzende Tests (peek/drop_at/fini-Drop), analog zur C-Semantik.

    #[test]
    fn peek_and_drop_at() {
        let mut queue = ReorderQueue::new(3, 100, SeqNumSize::Num16).unwrap();
        queue.push(100, 10u64);
        queue.push(102, 12); // 101 bleibt Lücke
        assert_eq!(queue.count(), 3);

        // peek per Index (nicht Seq-Nummer)
        assert_eq!(queue.peek(0).map(|(s, v)| (s, *v)), Some((100, 10)));
        assert!(queue.peek(1).is_none()); // Lücke
        assert_eq!(queue.peek(2).map(|(s, v)| (s, *v)), Some((102, 12)));
        assert!(queue.peek(3).is_none()); // >= count

        // peek_mut
        if let Some((_, v)) = queue.peek_mut(0) {
            *v += 1;
        }
        assert_eq!(queue.peek(0).map(|(_, v)| *v), Some(11));

        // drop_at(0) ruft den Drop-Callback und lässt die Lücke stehen
        let dropped = Arc::new(Mutex::new(Vec::new()));
        let dropped_cb = Arc::clone(&dropped);
        queue.set_drop_cb(Some(Box::new(move |s, e: u64| {
            dropped_cb.lock().unwrap().push((s, e));
        })));
        queue.drop_at(0);
        assert_eq!(queue.count(), 3); // begin/count wird (vorn) nicht geändert
        assert!(queue.peek(0).is_none());
        assert_eq!(*dropped.lock().unwrap(), vec![(100, 11)]);

        // drop_at am letzten Index reduziert count (Lücken bei 101 bleiben,
        // sind aber nicht mehr Teil der Zählung, da 102 das Fenster-Ende war)
        queue.drop_at(2);
        assert_eq!(*dropped.lock().unwrap(), vec![(100, 11), (102, 12)]);
        assert_eq!(queue.count(), 1);

        // pull liefert weiterhin nichts, da der Kopf (100) gelöscht wurde
        assert!(queue.pull().is_none());
    }

    #[test]
    fn fini_calls_drop_cb_for_remaining_elements() {
        let dropped = Arc::new(Mutex::new(Vec::new()));
        {
            let mut queue = ReorderQueue::new(2, 0, SeqNumSize::Num32).unwrap();
            let dropped_cb = Arc::clone(&dropped);
            queue.set_drop_cb(Some(Box::new(move |s, e: u64| {
                dropped_cb.lock().unwrap().push((s, e));
            })));
            queue.push(0, 42);
            queue.push(2, 44);
            queue.push(1, 43);
            // absichtlich nicht pullen
        }
        let mut v = dropped.lock().unwrap().clone();
        v.sort();
        assert_eq!(v, vec![(0, 42), (1, 43), (2, 44)]);
    }

    #[test]
    fn seq_num_size_32_wraps() {
        let mut queue = ReorderQueue::new(2, 0xfffffffe, SeqNumSize::Num32).unwrap();
        queue.push(0xfffffffe, 1u64);
        queue.push(0xffffffff, 2);
        queue.push(0, 3); // Wrap über die Serial-Number-Grenze
        queue.push(1, 4);
        assert_eq!(queue.count(), 4);
        assert_eq!(queue.pull().map(|(s, _)| s), Some(0xfffffffe));
        assert_eq!(queue.pull().map(|(s, _)| s), Some(0xffffffff));
        assert_eq!(queue.pull().map(|(s, _)| s), Some(0));
        assert_eq!(queue.pull().map(|(s, _)| s), Some(1));
    }
}
