use crate::{Range, RopeSlice, Selection, Syntax};

pub fn expand_selection(syntax: &Syntax, text: RopeSlice, selection: Selection) -> Selection {
    let cursor = &mut syntax.walk();

    selection.transform(|range| {
        let from = text.char_to_byte(range.from());
        let to = text.char_to_byte(range.to());

        let byte_range = from..to;
        cursor.reset_to_byte_range(from, to);

        while cursor.node().byte_range() == byte_range {
            if !cursor.goto_parent() {
                break;
            }
        }

        let node = cursor.node();
        let from = text.byte_to_char(node.start_byte());
        let to = text.byte_to_char(node.end_byte());

        Range::new(to, from).with_direction(range.direction())
    })
}

pub fn shrink_selection(syntax: &Syntax, text: RopeSlice, selection: Selection) -> Selection {
    selection.transform(move |range| {
        let (from, to) = range.into_byte_range(text);
        let mut cursor = syntax.walk();
        cursor.reset_to_byte_range(from, to);

        if let Some(node) = cursor.first_contained_child(&range, text) {
            return Range::from_node(node, text, range.direction());
        }

        range
    })
}

pub fn select_next_sibling(syntax: &Syntax, text: RopeSlice, selection: Selection) -> Selection {
    selection.transform(move |range| {
        let (from, to) = range.into_byte_range(text);
        let mut cursor = syntax.walk();
        cursor.reset_to_byte_range(from, to);

        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return range;
            }
        }

        Range::from_node(cursor.node(), text, range.direction())
    })
}

pub fn select_prev_sibling(syntax: &Syntax, text: RopeSlice, selection: Selection) -> Selection {
    selection.transform(move |range| {
        let (from, to) = range.into_byte_range(text);
        let mut cursor = syntax.walk();
        cursor.reset_to_byte_range(from, to);

        while !cursor.goto_prev_sibling() {
            if !cursor.goto_parent() {
                return range;
            }
        }

        Range::from_node(cursor.node(), text, range.direction())
    })
}
