#[allow(clippy::module_inception)]
mod html;

use std::fmt;

use arbitrary::{Arbitrary, Unstructured};
use html_escape::decode_html_entities_to_string;

// ADD NEW ENTRIES AT THE BOTTOM!
#[derive(Arbitrary, Debug, Clone, Copy)]
pub enum Scenario<'a> {
    String(&'a str),
    Char(char),
}

impl<'a> super::Scenario<'a> for Scenario<'a> {
    type RunError = std::convert::Infallible;

    fn new(data: &'a [u8]) -> Result<Self, arbitrary::Error> {
        Self::arbitrary_take_rest(Unstructured::new(data))
    }

    fn run(&self) -> Result<(), Self::RunError> {
        match *self {
            Scenario::String(src) => {
                let mut dest = String::with_capacity(src.len());
                html::write_escaped_str(&mut dest, src).unwrap();

                let mut unescaped = String::with_capacity(src.len());
                let unescaped = decode_html_entities_to_string(dest, &mut unescaped);
                assert_eq!(src, unescaped);
            }
            Scenario::Char(c) => {
                let mut dest = String::with_capacity(6);
                html::write_escaped_char(&mut dest, c).unwrap();

                let mut src = [0; 4];
                let src = c.encode_utf8(&mut src);
                let mut unescaped = String::with_capacity(4);
                let unescaped = decode_html_entities_to_string(dest, &mut unescaped);
                assert_eq!(src, unescaped);
            }
        }
        Ok(())
    }
}

impl fmt::Display for Scenario<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Scenario::String(src) => {
                write!(
                    f,
                    "\
#[test]
fn test() {{
    let mut dest = String::with_capacity({len});
    html::write_escaped_str(&mut dest, {src:?}).unwrap();

    let mut unescaped = String::with_capacity(src.len());
    let unescaped = html_escape::decode_html_entities_to_string(dest, &mut unescaped);
    assert_eq!(src, unescaped);
}}\
                    ",
                    len = src.len(),
                )
            }
            Scenario::Char(c) => {
                write!(
                    f,
                    "\
#[test]
fn test() {{
    let mut dest = String::with_capacity(6);
    html::write_escaped_char(&mut dest, {c:?}).unwrap();

    let mut src = [0; 4];
    let src = c.encode_utf8(&mut src);
    let mut unescaped = String::with_capacity(4);
    let unescaped = decode_html_entities_to_string(dest, &mut unescaped);
    assert_eq!(src, unescaped);
}}\
                    ",
                )
            }
        }
    }
}
