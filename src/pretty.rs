// Copyright 2021-2025 Martin Pool

//! Convert a token stream back to (reasonably) pretty Rust code in a string.

use proc_macro2::{Delimiter, TokenTree};
use quote::ToTokens;

/// Convert something to a pretty-printed string.
pub(crate) trait ToPrettyString {
    fn to_pretty_string(&self) -> String;
}

/// Convert a `TokenStream` representing some code to a reasonably formatted
/// string of Rust code.
///
/// `TokenStream` has a `to_string`, but it adds spaces in places that don't
/// look idiomatic, so this reimplements it in a way that looks better.
///
/// This is probably not correctly formatted for all Rust syntax, and only tries
/// to cover cases that can emerge from the code we generate.
impl<T> ToPrettyString for T
where
    T: ToTokens,
{
    fn to_pretty_string(&self) -> String {
        let mut b = String::with_capacity(200);
        write_pretty(self.to_token_stream(), &mut b);
        b
    }
}

/// Append the pretty-printed form of `ts` to `b`.
///
/// Groups recurse into the same buffer rather than into a fresh `String` that
/// is then copied in: a nested type like `Result<Option<Vec<String>>, E>`
/// would otherwise allocate once per level.
///
/// The spacing rules look only at the text this token stream has produced
/// itself, not at whatever the caller had already written, so every position
/// test is relative to `start`. That matters for a group whose delimiter is
/// `Delimiter::None`, which emits no opening character to separate it from the
/// text before it.
fn write_pretty(ts: proc_macro2::TokenStream, b: &mut String) {
    use TokenTree::{Group, Ident, Literal, Punct};
    let start = b.len();
    let mut ts = ts.into_iter().peekable();
    while let Some(tt) = ts.next() {
        match tt {
            Punct(p) => {
                let pc = p.as_char();
                let emitted = &b[start..];
                if pc == '|'
                    && !emitted.is_empty()
                    && !emitted.ends_with(' ')
                    && !emitted.ends_with('|')
                {
                    // Generally have spaces around '|' in match arms (or bitwise expressions)
                    b.push(' ');
                }
                b.push(pc);
                if ts.peek().is_some() && (b[start..].ends_with("->") || pc == ',' || pc == ';') {
                    b.push(' ');
                }
            }
            Ident(_) | Literal(_) => {
                let emitted = &b[start..];
                if emitted.ends_with('=') || emitted.ends_with("=>") || emitted.ends_with('|') {
                    b.push(' ');
                }
                match tt {
                    Literal(l) => b.push_str(&l.to_string()),
                    Ident(i) => b.push_str(&i.to_string()),
                    _ => unreachable!(),
                }
                if let Some(next) = ts.peek() {
                    match next {
                        Ident(_) | Literal(_) => b.push(' '),
                        Punct(p) => match p.as_char() {
                            ',' | ';' | '<' | '>' | ':' | '.' | '!' => (),
                            _ => b.push(' '),
                        },
                        Group(_) => (),
                    }
                }
            }
            Group(g) => {
                match g.delimiter() {
                    Delimiter::Brace => b.push('{'),
                    Delimiter::Bracket => b.push('['),
                    Delimiter::Parenthesis => b.push('('),
                    Delimiter::None => (),
                }
                write_pretty(g.stream(), b);
                match g.delimiter() {
                    Delimiter::Brace => b.push('}'),
                    Delimiter::Bracket => b.push(']'),
                    Delimiter::Parenthesis => b.push(')'),
                    Delimiter::None => (),
                }
            }
        }
    }
    debug_assert!(
        !b[start..].ends_with(' '),
        "generated a trailing space: {:?}",
        &b[start..]
    );
}

#[cfg(test)]
mod test {
    use pretty_assertions::assert_eq;
    use quote::quote;

    use super::ToPrettyString;

    #[test]
    fn pretty_format_examples() {
        assert_eq!(
            quote! {
                // Nonsense rust but a big salad of syntax
                <impl Iterator for MergeTrees < AE , BE , AIT , BIT > > :: next
                -> Option < Self ::  Item >
            }
            .to_pretty_string(),
            "<impl Iterator for MergeTrees<AE, BE, AIT, BIT>>::next -> Option<Self::Item>"
        );
        assert_eq!(
            quote! { Lex < 'buf >::take }.to_pretty_string(),
            "Lex<'buf>::take"
        );
    }

    #[test]
    fn format_trait_with_assoc_type() {
        assert_eq!(
            quote! { impl Iterator < Item = String > }.to_pretty_string(),
            "impl Iterator<Item = String>"
        );
    }

    #[test]
    fn pretty_match_guard() {
        assert_eq!(
            quote! { "warning"
            | "error" }
            .to_pretty_string(),
            r#""warning" | "error""#,
        );
    }

    #[test]
    fn preserve_boolean_or() {
        assert_eq!(quote! { fair || foul }.to_pretty_string(), "fair || foul");
    }

    #[test]
    fn format_thick_arrow() {
        assert_eq!(quote! { a => b }.to_pretty_string(), "a => b");
    }
}
