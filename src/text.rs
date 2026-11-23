//! Text as the editor and Prettier each count it. The editor's positions are
//! lines and UTF-8 byte columns; Prettier's ranges are UTF-16 offsets into
//! the whole text, and its errors 1-based lines and columns.

use flareite_extension_api::types::{Position, Range, TextEdit};

/// The byte offset of `position` in `text`, clamped to its line (and the
/// text) and moved back onto a character boundary.
pub fn offset(text: &str, position: Position) -> usize {
  let mut start = 0;
  for _ in 0..position.line {
    match text[start..].find('\n') {
      Some(end) => start += end + 1,
      None => return text.len(),
    }
  }
  let line_end = text[start..]
    .find('\n')
    .map_or(text.len(), |end| start + end);
  let mut at = (start + position.column as usize).min(line_end);
  while !text.is_char_boundary(at) {
    at -= 1;
  }
  at
}

/// The position of byte offset `at` in `text`.
pub fn position(text: &str, at: usize) -> Position {
  let before = &text[..at];
  let line = before.matches('\n').count();
  let line_start = before.rfind('\n').map_or(0, |index| index + 1);
  Position {
    line: u32::try_from(line).unwrap_or(u32::MAX),
    column: u32::try_from(at - line_start).unwrap_or(u32::MAX),
  }
}

/// The UTF-16 offset of byte offset `at`, as JavaScript counts.
pub fn utf16(text: &str, at: usize) -> usize {
  text[..at].encode_utf16().count()
}

/// The one edit that turns `old` into `new`: what's between their common
/// start and end. `None` when they're the same.
pub fn edit(old: &str, new: &str) -> Option<TextEdit> {
  if old == new {
    return None;
  }
  let mut prefix = old
    .bytes()
    .zip(new.bytes())
    .take_while(|(a, b)| a == b)
    .count();
  while !old.is_char_boundary(prefix) {
    prefix -= 1;
  }
  let longest = old.len().min(new.len()) - prefix;
  let mut suffix = old
    .bytes()
    .rev()
    .zip(new.bytes().rev())
    .take(longest)
    .take_while(|(a, b)| a == b)
    .count();
  while !old.is_char_boundary(old.len() - suffix) {
    suffix -= 1;
  }
  let end = old.len() - suffix;
  Some(TextEdit {
    range: Range {
      start: position(old, prefix),
      end: position(old, end),
    },
    text: new[prefix..new.len() - suffix].to_owned(),
  })
}

/// The range a Prettier error points at: from its 1-based line and column
/// (UTF-16 units into the line) to its end if given, or the character
/// there. Clamped to the text.
pub fn error_range(text: &str, line: u32, column: u32, end: Option<(u32, u32)>) -> Range {
  let start = at_prettier(text, line, column);
  let end = match end {
    Some((end_line, end_column)) => {
      let end = at_prettier(text, end_line, end_column);
      if (end.line, end.column) > (start.line, start.column) {
        Some(end)
      } else {
        None
      }
    }
    None => None,
  };
  let end = end.unwrap_or_else(|| {
    // The character at the start, if the line has one there.
    let from = offset(text, start);
    let next = text[from..]
      .chars()
      .next()
      .filter(|character| *character != '\n')
      .map_or(from, |character| from + character.len_utf8());
    position(text, next)
  });
  Range { start, end }
}

/// The editor's position for Prettier's 1-based `line` and `column`.
fn at_prettier(text: &str, line: u32, column: u32) -> Position {
  let line = line.saturating_sub(1);
  let start = offset(text, Position { line, column: 0 });
  let line_text = text[start..].split('\n').next().unwrap_or_default();
  let mut units = 0;
  let mut bytes = 0;
  for character in line_text.chars() {
    if units >= column.saturating_sub(1) as usize {
      break;
    }
    units += character.len_utf16();
    bytes += character.len_utf8();
  }
  position(text, start + bytes)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn at(line: u32, column: u32) -> Position {
    Position { line, column }
  }

  #[test]
  fn offsets_and_positions_count_bytes_on_lines() {
    let text = "a\nbé c\n\nd";
    assert_eq!(offset(text, at(0, 0)), 0);
    assert_eq!(offset(text, at(1, 3)), 5);
    // Inside é: back to its start.
    assert_eq!(offset(text, at(1, 2)), 3);
    // Past the line's end, or the text's.
    assert_eq!(offset(text, at(1, 99)), 7);
    assert_eq!(offset(text, at(9, 0)), text.len());
    for at in [0, 2, 3, 5, 7, 8, 9, 10] {
      assert_eq!(offset(text, position(text, at)), at);
    }
    assert_eq!(position(text, 9), Position { line: 3, column: 0 });
  }

  #[test]
  fn utf16_counts_as_javascript_does() {
    let text = "é😀x";
    assert_eq!(utf16(text, 2), 1);
    assert_eq!(utf16(text, 6), 3);
    assert_eq!(utf16(text, text.len()), 4);
  }

  fn apply(old: &str, edit: &TextEdit) -> String {
    let start = offset(old, edit.range.start);
    let end = offset(old, edit.range.end);
    format!("{}{}{}", &old[..start], edit.text, &old[end..])
  }

  #[test]
  fn one_edit_replaces_what_changed() {
    let cases = [
      ("const a = {b:1}\n", "const a = { b: 1 }\n"),
      ("x\n\n\ny\n", "x\n\ny\n"),
      ("aaa", "aaaa"),
      ("aaaa", "aaa"),
      ("", "a\n"),
      ("a\n", ""),
      ("é", "è"),
      ("😀😀", "😀😃😀"),
      ("f(1)\nf( 2 )\nf(3)\n", "f(1)\nf(2)\nf(3)\n"),
    ];
    for (old, new) in cases {
      let edit = edit(old, new).expect("they differ");
      assert_eq!(apply(old, &edit), new, "{old:?} -> {new:?}: {edit:?}");
    }
    assert!(edit("same", "same").is_none());
    let edit = edit("a\nb( 2 )\nc\n", "a\nb(2)\nc\n").unwrap();
    // Between the common start, "a\nb(", and end, ")\nc\n".
    assert_eq!(edit.range.start, at(1, 2));
    assert_eq!(edit.range.end, at(1, 5));
    assert_eq!(edit.text, "2");
  }

  #[test]
  fn errors_point_at_the_character_prettier_names() {
    let text = "const a = {b:1,,}\nlet é😀 = (\n";
    // Prettier's (1:16): the second comma.
    let range = error_range(text, 1, 16, None);
    assert_eq!((range.start, range.end), (at(0, 15), at(0, 16)));
    // Columns count UTF-16 units: column 8 is after "let é😀" (7 units, 10
    // bytes), and column 10 after the " =" that follows.
    let range = error_range(text, 2, 8, Some((2, 10)));
    assert_eq!((range.start, range.end), (at(1, 10), at(1, 12)));
    // At a line's end, or past the text: an empty range there.
    let range = error_range(text, 2, 99, None);
    assert_eq!(range.start, range.end);
    let range = error_range(text, 9, 1, None);
    assert_eq!(range.start, position(text, text.len()));
  }
}
