//! Morse code tree and glyph table. SPEC §4.4.

use std::sync::OnceLock;

/// A single keyed element. Ord derives Dit < Dah (SPEC §6.5 tie-break).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Element {
    Dit,
    Dah,
}

/// Prosigns emitted as text tokens in the JSON stream, dropped from
/// telnet-facing text. SPEC §4.4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Prosign {
    Ar,
    Sk,
    As,
    Sn,
    /// Operator error (........); synthesized by the beam stage, not in the tree.
    Err,
}

impl Prosign {
    pub fn token(&self) -> &'static str {
        match self {
            Prosign::Ar => "<AR>",
            Prosign::Sk => "<SK>",
            Prosign::As => "<AS>",
            Prosign::Sn => "<SN>",
            Prosign::Err => "<ERR>",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Glyph {
    Char(char),
    Prosign(Prosign),
}

impl Glyph {
    /// The plain-text rendering, or None for prosigns (dropped from text).
    pub fn text_char(&self) -> Option<char> {
        match self {
            Glyph::Char(c) => Some(*c),
            Glyph::Prosign(_) => None,
        }
    }
}

pub type NodeId = u16;

/// (pattern, glyph). SPEC §4.4 standard table + prosign terminals.
/// '+' is intentionally absent: .-.-. carries the AR prosign (pinned decision 7).
/// BT (-...-) and KN (-.--.) are the '=' and '(' nodes per SPEC.
pub(crate) const TABLE: &[(&str, Glyph)] = &[
    (".-", Glyph::Char('A')),
    ("-...", Glyph::Char('B')),
    ("-.-.", Glyph::Char('C')),
    ("-..", Glyph::Char('D')),
    (".", Glyph::Char('E')),
    ("..-.", Glyph::Char('F')),
    ("--.", Glyph::Char('G')),
    ("....", Glyph::Char('H')),
    ("..", Glyph::Char('I')),
    (".---", Glyph::Char('J')),
    ("-.-", Glyph::Char('K')),
    (".-..", Glyph::Char('L')),
    ("--", Glyph::Char('M')),
    ("-.", Glyph::Char('N')),
    ("---", Glyph::Char('O')),
    (".--.", Glyph::Char('P')),
    ("--.-", Glyph::Char('Q')),
    (".-.", Glyph::Char('R')),
    ("...", Glyph::Char('S')),
    ("-", Glyph::Char('T')),
    ("..-", Glyph::Char('U')),
    ("...-", Glyph::Char('V')),
    (".--", Glyph::Char('W')),
    ("-..-", Glyph::Char('X')),
    ("-.--", Glyph::Char('Y')),
    ("--..", Glyph::Char('Z')),
    ("-----", Glyph::Char('0')),
    (".----", Glyph::Char('1')),
    ("..---", Glyph::Char('2')),
    ("...--", Glyph::Char('3')),
    ("....-", Glyph::Char('4')),
    (".....", Glyph::Char('5')),
    ("-....", Glyph::Char('6')),
    ("--...", Glyph::Char('7')),
    ("---..", Glyph::Char('8')),
    ("----.", Glyph::Char('9')),
    (".-.-.-", Glyph::Char('.')),
    ("--..--", Glyph::Char(',')),
    ("..--..", Glyph::Char('?')),
    ("-..-.", Glyph::Char('/')),
    ("-...-", Glyph::Char('=')), // BT
    ("-....-", Glyph::Char('-')),
    ("-.--.", Glyph::Char('(')), // KN
    ("-.--.-", Glyph::Char(')')),
    (".--.-.", Glyph::Char('@')),
    ("---...", Glyph::Char(':')),
    ("-.-.-.", Glyph::Char(';')),
    (".----.", Glyph::Char('\'')),
    (".-..-.", Glyph::Char('"')),
    ("..--.-", Glyph::Char('_')),
    ("...-..-", Glyph::Char('$')),
    ("-.-.--", Glyph::Char('!')),
    (".-.-.", Glyph::Prosign(Prosign::Ar)),
    ("...-.-", Glyph::Prosign(Prosign::Sk)),
    (".-...", Glyph::Prosign(Prosign::As)),
    ("...-.", Glyph::Prosign(Prosign::Sn)),
];

#[derive(Debug, Clone, Copy)]
struct Node {
    glyph: Option<Glyph>,
    children: [Option<NodeId>; 2], // [dit, dah]
}

pub struct MorseTree {
    nodes: Vec<Node>,
}

impl MorseTree {
    pub const ROOT: NodeId = 0;

    pub fn shared() -> &'static MorseTree {
        static TREE: OnceLock<MorseTree> = OnceLock::new();
        TREE.get_or_init(MorseTree::build)
    }

    fn build() -> MorseTree {
        let mut nodes = vec![Node {
            glyph: None,
            children: [None, None],
        }];
        for &(pattern, glyph) in TABLE {
            let mut cur: NodeId = Self::ROOT;
            for c in pattern.chars() {
                let idx = if c == '.' { 0 } else { 1 };
                cur = match nodes[cur as usize].children[idx] {
                    Some(next) => next,
                    None => {
                        let id = nodes.len() as NodeId;
                        nodes.push(Node {
                            glyph: None,
                            children: [None, None],
                        });
                        nodes[cur as usize].children[idx] = Some(id);
                        id
                    }
                };
            }
            let slot = &mut nodes[cur as usize].glyph;
            assert!(slot.is_none(), "duplicate Morse pattern {pattern}");
            *slot = Some(glyph);
        }
        MorseTree { nodes }
    }

    pub fn child(&self, n: NodeId, e: Element) -> Option<NodeId> {
        let idx = match e {
            Element::Dit => 0,
            Element::Dah => 1,
        };
        self.nodes[n as usize].children[idx]
    }

    pub fn glyph(&self, n: NodeId) -> Option<Glyph> {
        self.nodes[n as usize].glyph
    }
}

/// Encoding lookup for the testkit keyer: 'W' -> ".--" (case-insensitive).
pub fn pattern_for(c: char) -> Option<&'static str> {
    let up = c.to_ascii_uppercase();
    TABLE
        .iter()
        .find(|(_, g)| *g == Glyph::Char(up))
        .map(|(p, _)| *p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn walk(pattern: &str) -> Option<Glyph> {
        let t = MorseTree::shared();
        let mut n = MorseTree::ROOT;
        for c in pattern.chars() {
            let e = if c == '.' { Element::Dit } else { Element::Dah };
            n = t.child(n, e)?;
        }
        t.glyph(n)
    }

    #[test]
    fn letters_digits_decode() {
        assert_eq!(walk(".-"), Some(Glyph::Char('A')));
        assert_eq!(walk("-.-."), Some(Glyph::Char('C')));
        assert_eq!(walk(".--"), Some(Glyph::Char('W')));
        assert_eq!(walk(".----"), Some(Glyph::Char('1')));
        assert_eq!(walk("-----"), Some(Glyph::Char('0')));
    }

    #[test]
    fn shared_nodes_emit_spec_glyph() {
        // SPEC §4.4: BT (-...-) emits '='; KN (-.--.) emits '('.
        assert_eq!(walk("-...-"), Some(Glyph::Char('=')));
        assert_eq!(walk("-.--."), Some(Glyph::Char('(')));
        // Pinned decision 7: .-.-. is the AR prosign (not '+').
        assert_eq!(walk(".-.-."), Some(Glyph::Prosign(Prosign::Ar)));
        assert_eq!(walk("...-.-"), Some(Glyph::Prosign(Prosign::Sk)));
        assert_eq!(walk(".-..."), Some(Glyph::Prosign(Prosign::As)));
        assert_eq!(walk("...-."), Some(Glyph::Prosign(Prosign::Sn)));
    }

    #[test]
    fn punctuation_decodes() {
        for (p, c) in [
            (".-.-.-", '.'),
            ("--..--", ','),
            ("..--..", '?'),
            ("-..-.", '/'),
            ("-....-", '-'),
            ("-.--.-", ')'),
            (".--.-.", '@'),
            ("---...", ':'),
            ("-.-.-.", ';'),
            (".----.", '\''),
            (".-..-.", '"'),
            ("..--.-", '_'),
            ("...-..-", '$'),
            ("-.-.--", '!'),
        ] {
            assert_eq!(walk(p), Some(Glyph::Char(c)), "pattern {p}");
        }
    }

    #[test]
    fn interior_nodes_are_glyphless_and_deep_paths_fall_off() {
        let t = MorseTree::shared();
        // "..-..": interior/absent paths must not panic.
        assert_eq!(walk("........"), None); // falls off the tree (max depth 7)
        assert!(t.glyph(MorseTree::ROOT).is_none());
    }

    #[test]
    fn pattern_for_encodes() {
        assert_eq!(pattern_for('W'), Some(".--"));
        assert_eq!(pattern_for('w'), Some(".--"));
        assert_eq!(pattern_for('5'), Some("....."));
        assert_eq!(pattern_for('#'), None);
    }

    #[test]
    fn element_order_is_dit_before_dah() {
        // SPEC §6.5 tie-break depends on this.
        assert!(Element::Dit < Element::Dah);
        assert!(vec![Element::Dit] < vec![Element::Dah]);
    }

    #[test]
    fn no_pattern_exceeds_seven_elements() {
        for (p, _) in TABLE {
            assert!(p.len() <= 7, "pattern {p} too long");
        }
    }
}
