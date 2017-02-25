/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#![crate_name = "cssparser"]
#![crate_type = "rlib"]

#![cfg_attr(feature = "bench", feature(test))]
#![deny(missing_docs)]

/*!

Implementation of [CSS Syntax Module Level 3](https://drafts.csswg.org/css-syntax/) for Rust.

# Input

Everything is based on `Parser` objects, which borrow a `&str` input.
If you have bytes (from a file, the network, or something)
and want to support character encodings other than UTF-8,
see the `stylesheet_encoding` function,
which can be used together with rust-encoding or encoding-rs.

# Conventions for parsing functions

* Take (at least) a `input: &mut cssparser::Parser` parameter
* Return `Result<_, ()>`
* When returning `Ok(_)`,
  the function must have consume exactly the amount of input that represents the parsed value.
* When returning `Err(())`, any amount of input may have been consumed.

As a consequence, when calling another parsing function, either:

* Any `Err(())` return value must be propagated.
  This happens by definition for tail calls,
  and can otherwise be done with the `try!` macro.
* Or the call must be wrapped in a `Parser::try` call.
  `try` takes a closure that takes a `Parser` and returns a `Result`,
  calls it once,
  and returns itself that same result.
  If the result is `Err`,
  it restores the position inside the input to the one saved before calling the closure.

Examples:

```{rust,ignore}
// 'none' | <image>
fn parse_background_image(context: &ParserContext, input: &mut Parser)
                                    -> Result<Option<Image>, ()> {
    if input.try(|input| input.expect_ident_matching("none")).is_ok() {
        Ok(None)
    } else {
        Image::parse(context, input).map(Some)  // tail call
    }
}
```

```{rust,ignore}
// [ <length> | <percentage> ] [ <length> | <percentage> ]?
fn parse_border_spacing(_context: &ParserContext, input: &mut Parser)
                          -> Result<(LengthOrPercentage, LengthOrPercentage), ()> {
    let first = try!(LengthOrPercentage::parse);
    let second = input.try(LengthOrPercentage::parse).unwrap_or(first);
    (first, second)
}
```

*/

#![recursion_limit="200"]  // For color::parse_color_keyword

#[macro_use] extern crate cssparser_macros;
#[macro_use] extern crate matches;
extern crate phf;
#[cfg(test)] extern crate encoding_rs;
#[cfg(test)] extern crate tempdir;
#[cfg(test)] extern crate rustc_serialize;
#[cfg(feature = "serde")] extern crate serde;
#[cfg(feature = "heapsize")] #[macro_use] extern crate heapsize;

pub use tokenizer::{Token, NumericValue, PercentageValue, SourceLocation};
pub use rules_and_declarations::{parse_important};
pub use rules_and_declarations::{DeclarationParser, DeclarationListParser, parse_one_declaration};
pub use rules_and_declarations::{RuleListParser, parse_one_rule};
pub use rules_and_declarations::{AtRuleType, QualifiedRuleParser, AtRuleParser};
pub use from_bytes::{stylesheet_encoding, EncodingSupport};
pub use color::{RGBA, Color, parse_color_keyword};
pub use nth::parse_nth;
pub use serializer::{ToCss, CssStringWriter, serialize_identifier, serialize_string, TokenSerializationType};
pub use parser::{Parser, Delimiter, Delimiters, SourcePosition};
pub use unicode_range::UnicodeRange;


/**

This macro is equivalent to a `match` expression on an `&str` value,
but matching is case-insensitive in the ASCII range.

Usage example:

```{rust,ignore}
match_ignore_ascii_case! { string,
    "foo" => Some(Foo),
    "bar" => Some(Bar),
    "baz" => Some(Baz),
    _ => None
}
```

*/
#[macro_export]
macro_rules! match_ignore_ascii_case {
    // parse the last case plus the fallback
    (@inner $value:expr, ($string:expr => $result:expr, _ => $fallback:expr) -> ($($parsed:tt)*) ) => {
        match_ignore_ascii_case!(@inner $value, () -> ($($parsed)* ($string => $result)) $fallback)
    };

    // parse a case (not the last one)
    (@inner $value:expr, ($string:expr => $result:expr, $($rest:tt)*) -> ($($parsed:tt)*) ) => {
        match_ignore_ascii_case!(@inner $value, ($($rest)*) -> ($($parsed)* ($string => $result)))
    };

    // finished parsing
    (@inner $value:expr, () -> ($(($string:expr => $result:expr))*) $fallback:expr ) => {
        {
            _cssparser_internal__max_len!($value => lowercase, $($string),+);
            match lowercase {
                $(
                    Some($string) => $result,
                )+
                _ => $fallback
            }
        }
    };

    // entry point, start parsing
    ( $value:expr, $($rest:tt)* ) => {
        match_ignore_ascii_case!(@inner $value, ($($rest)*) -> ())
    };
}

#[macro_export]
macro_rules! ascii_case_insensitive_phf_map {
    ($Name: ident : Map<$ValueType: ty> = {
        $( $key: expr => $value: expr, )*
    }) => {
        #[derive(cssparser__phf_map)]
        #[cssparser__phf_map__kv_pairs(
            $(
                key = $key,
                value = $value
            ),+
        )]
        struct $Name($ValueType);

        impl $Name {
            #[inline]
            fn get(input: &str) -> Option<&'static $ValueType> {
                _cssparser_internal__max_len!(input => lowercase, $($key),+);
                lowercase.and_then(|string| $Name::map().get(string))
            }
        }
    }
}

#[macro_export]
macro_rules! _cssparser_internal__max_len {
    ($input: expr => $output: ident, $($string: expr),+) => {
        #[derive(cssparser__match_ignore_ascii_case__max_len)]
        #[cssparser__match_ignore_ascii_case__data($(string = $string),+)]
        #[allow(dead_code)]
        struct Dummy;

        // MAX_LENGTH is generated by cssparser__match_ignore_ascii_case__max_len
        let mut buffer: [u8; MAX_LENGTH] =
            // `buffer` is only used in `_match_ignore_ascii_case__to_lowercase`,
            // which initializes with `copy_from_slice` the part of the buffer it uses,
            // before it uses it.
            unsafe {
                ::std::mem::uninitialized()
            };
        let $output = $crate::_match_ignore_ascii_case__to_lowercase(&mut buffer, $input);
    }
}


/// Implementation detail of macros.
#[doc(hidden)]
#[allow(non_snake_case)]
pub fn _match_ignore_ascii_case__to_lowercase<'a>(buffer: &'a mut [u8], input: &'a str) -> Option<&'a str> {
    if let Some(buffer) = buffer.get_mut(..input.len()) {
        if let Some(first_uppercase) = input.bytes().position(|byte| matches!(byte, b'A'...b'Z')) {
            buffer.copy_from_slice(input.as_bytes());
            std::ascii::AsciiExt::make_ascii_lowercase(&mut buffer[first_uppercase..]);
            // `buffer` was initialized to a copy of `input` (which is &str so well-formed UTF-8)
            // then lowercased (which preserves UTF-8 well-formedness)
            unsafe {
                Some(::std::str::from_utf8_unchecked(buffer))
            }
        } else {
            // Input is already lower-case
            Some(input)
        }
    } else {
        // Input is longer than buffer, which has the length of the longest expected string:
        // none of the expected strings would match.
        None
    }
}

mod rules_and_declarations;

#[cfg(feature = "dummy_match_byte")]
macro_rules! match_byte {
    ($value:expr, $($rest:tt)* ) => {
        match $value {
            $(
                $rest
            )+
        }
    };
}

#[cfg(feature = "dummy_match_byte")]
mod tokenizer;

#[cfg(not(feature = "dummy_match_byte"))]
mod tokenizer {
    include!(concat!(env!("OUT_DIR"), "/tokenizer.rs"));
}
mod parser;
mod from_bytes;
mod color;
mod nth;
mod serializer;
mod unicode_range;

#[cfg(test)]
mod tests;
