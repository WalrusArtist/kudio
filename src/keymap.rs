//! Computer-keyboard layouts.
//!
//! Piano mode uses the classic "tracker" layout: two rows of keys form two
//! overlapping piano octaves.
//!
//! ```text
//!  upper:  2 3   5 6 7   9 0   =        (black keys)
//!         q w e r t y u i o p [ ]       (white keys, octave + 1)
//!  lower:  s d   g h j   l ;            (black keys)
//!         z x c v b n m , . /           (white keys, base octave)
//! ```

/// Undo the Shift layer so `Q`, `@`, `:` etc. map to the same physical key
/// as `q`, `2`, `;` (US layout).
pub fn base_char(c: char) -> char {
    match c {
        'A'..='Z' => c.to_ascii_lowercase(),
        '!' => '1',
        '@' => '2',
        '#' => '3',
        '$' => '4',
        '%' => '5',
        '^' => '6',
        '&' => '7',
        '*' => '8',
        '(' => '9',
        ')' => '0',
        '_' => '-',
        '+' => '=',
        '{' => '[',
        '}' => ']',
        ':' => ';',
        '"' => '\'',
        '<' => ',',
        '>' => '.',
        '?' => '/',
        '~' => '`',
        '|' => '\\',
        c => c,
    }
}

/// Semitone offset of a piano key relative to the base C, if it is one.
pub fn piano_offset(c: char) -> Option<i32> {
    Some(match c {
        'z' => 0,
        's' => 1,
        'x' => 2,
        'd' => 3,
        'c' => 4,
        'v' => 5,
        'g' => 6,
        'b' => 7,
        'h' => 8,
        'n' => 9,
        'j' => 10,
        'm' => 11,
        ',' => 12,
        'l' => 13,
        '.' => 14,
        ';' => 15,
        '/' => 16,
        'q' => 12,
        '2' => 13,
        'w' => 14,
        '3' => 15,
        'e' => 16,
        'r' => 17,
        '5' => 18,
        't' => 19,
        '6' => 20,
        'y' => 21,
        '7' => 22,
        'u' => 23,
        'i' => 24,
        '9' => 25,
        'o' => 26,
        '0' => 27,
        'p' => 28,
        '[' => 29,
        '=' => 30,
        ']' => 31,
        _ => return None,
    })
}

/// Control mode: the top letter row raises, the home row lowers, one column
/// per parameter slot on the current page.
pub const UP_KEYS: [char; 10] = ['q', 'w', 'e', 'r', 't', 'y', 'u', 'i', 'o', 'p'];
pub const DOWN_KEYS: [char; 10] = ['a', 's', 'd', 'f', 'g', 'h', 'j', 'k', 'l', ';'];

pub fn slot_key(c: char) -> Option<(usize, i32)> {
    if let Some(i) = UP_KEYS.iter().position(|&k| k == c) {
        return Some((i, 1));
    }
    DOWN_KEYS.iter().position(|&k| k == c).map(|i| (i, -1))
}

const NAMES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];

pub fn note_name(n: u8) -> String {
    format!("{}{}", NAMES[n as usize % 12], n as i32 / 12 - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_names() {
        assert_eq!(note_name(60), "C4");
        assert_eq!(note_name(69), "A4");
        assert_eq!(note_name(61), "C#4");
    }

    #[test]
    fn shift_is_normalized() {
        assert_eq!(base_char('Q'), 'q');
        assert_eq!(base_char('@'), '2');
        assert_eq!(piano_offset(base_char(':')), Some(15));
    }

    #[test]
    fn slots() {
        assert_eq!(slot_key('q'), Some((0, 1)));
        assert_eq!(slot_key(';'), Some((9, -1)));
        assert_eq!(slot_key('z'), None);
    }
}
