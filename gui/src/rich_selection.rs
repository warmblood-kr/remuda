//! Experimental document-order selection. Not connected to the live view yet.
use std::ops::Range;
use unicode_segmentation::GraphemeCursor;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentKey { pub pane_id: u64, pub path: String, pub version: String, pub epoch: u64 }

#[derive(Default)]
pub struct Selection {
    pub key: Option<DocumentKey>,
    text: String,
    parsed: Option<Vec<remuda_gui::markdown::Block>>,
    pub anchor: Option<usize>,
    pub head: Option<usize>,
    pub dragging: bool,
    pub moved: bool,
    pub pointer_start: Option<(f32,f32)>,
}
impl Selection {
    pub fn begin_document(&mut self, key: DocumentKey) {
        if self.key.as_ref() != Some(&key) { self.clear(); self.key = Some(key); }
        self.text.clear();
    }
    pub fn blocks(&mut self, source: &str) -> Vec<remuda_gui::markdown::Block> {
        self.parsed.get_or_insert_with(|| remuda_gui::markdown::parse(source)).clone()
    }
    pub fn add_piece(&mut self, text: &str) -> Range<usize> {
        let start = self.text.len(); self.text.push_str(text); start..self.text.len()
    }
    pub fn start(&mut self, offset: usize) {
        let offset = self.snap(offset); self.anchor = Some(offset); self.head = Some(offset);
        self.dragging = true; self.moved = false;
    }
    pub fn extend(&mut self, offset: usize, physically_moved: bool) {
        if !self.dragging { return; } self.head = Some(self.snap(offset)); self.moved |= physically_moved;
    }
    pub fn note_pointer(&mut self, x: f32, y: f32) {
        if let Some((sx,sy)) = self.pointer_start { self.moved |= (x-sx).abs() + (y-sy).abs() >= 3.0; }
    }
    pub fn end(&mut self) { self.dragging = false; self.pointer_start = None; }
    pub fn copy(&self, key: &DocumentKey) -> Option<&str> {
        if self.key.as_ref() != Some(key) || self.dragging { return None; }
        let (a,b) = (self.anchor?, self.head?); if a == b { return None; }
        self.text.get(a.min(b)..a.max(b))
    }
    pub fn selected_range(&self, range: Range<usize>) -> Option<Range<usize>> {
        let (a,b) = (self.anchor?, self.head?); let lo = a.min(b).max(range.start); let hi = a.max(b).min(range.end);
        (lo < hi).then_some(lo.saturating_sub(range.start)..hi.saturating_sub(range.start))
    }
    pub fn clear(&mut self) { self.key=None; self.text.clear(); self.parsed=None; self.anchor=None; self.head=None; self.dragging=false; self.moved=false; self.pointer_start=None; }
    fn snap(&self, offset: usize) -> usize {
        let mut offset = offset.min(self.text.len());
        while !self.text.is_char_boundary(offset) { offset -= 1; }
        let mut cursor = GraphemeCursor::new(offset, self.text.len(), true);
        if cursor.is_boundary(&self.text, 0).unwrap_or(false) { offset }
        else { cursor.prev_boundary(&self.text, 0).ok().flatten().unwrap_or(0) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(pane_id:u64, version:&str)->DocumentKey { DocumentKey { pane_id, path:"guide.md".into(), version:version.into(), epoch:0 } }
    #[test] fn selected_visible_pieces_are_exact_and_identity_bound() {
        let mut s=Selection::default(); let k=key(1,"v1"); s.begin_document(k.clone());
        s.add_piece("  head 한"); s.add_piece("\n"); s.add_piece("    code e\u{301}  ");
        s.start(2); s.extend(usize::MAX,true); s.end();
        assert_eq!(s.copy(&k),Some("head 한\n    code e\u{301}  "));
        assert_eq!(s.copy(&key(2,"v1")),None); assert_eq!(s.copy(&key(1,"v2")),None);
        s.begin_document(key(1,"v2")); s.add_piece("new"); assert_eq!(s.copy(&key(1,"v2")),None);
    }
    #[test] fn reverse_and_grapheme_selection() {
        let mut s=Selection::default(); let k=key(1,"v1"); s.begin_document(k.clone()); s.add_piece("Ae\u{301}👩\u{200d}💻Z");
        s.start(s.text.len()-1); s.extend(2,true); s.end(); assert_eq!(s.copy(&k),Some("e\u{301}👩\u{200d}💻"));
    }
}
