//! Parser for updatedb.conf, following the grammar of plocate's conf.cpp.

use std::fs;
use std::io;
use std::iter::Peekable;
use std::path::Path;
use std::str::Chars;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct UpdatedbConf {
    pub prunepaths: Vec<String>,
    pub prunenames: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Var {
    PruneBindMounts,
    Prunefs,
    Prunenames,
    Prunepaths,
}

enum Token {
    Eof,
    Eol,
    Identifier,
    Equal,
    Quoted,
    Other,
    Keyword(Var),
}

struct Lexer<'a> {
    chars: Peekable<Chars<'a>>,
    path: &'a str,
    line: u32,
    /// Line the token being returned started on, which is what errors name.
    token_line: u32,
    buf: String,
}

impl<'a> Lexer<'a> {
    fn new(text: &'a str, path: &'a str) -> Self {
        Lexer {
            chars: text
                .chars()
                .peekable(),
            path,
            line: 1,
            token_line: 1,
            buf: String::new(),
        }
    }

    fn error(&self, msg: &str) -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}:{}: {msg}", self.path, self.token_line),
        )
    }

    fn next_token(&mut self) -> io::Result<Token> {
        self.buf
            .clear();
        self.token_line = self.line;
        let c = loop {
            match self
                .chars
                .next()
            {
                None => return Ok(Token::Eof),
                Some(c) if c != '\n' && c.is_whitespace() => continue,
                Some(c) => break c,
            }
        };
        match c {
            '#' => {
                loop {
                    match self
                        .chars
                        .next()
                    {
                        None => return Ok(Token::Eof),
                        Some('\n') => break,
                        Some(_) => continue,
                    }
                }
                self.line += 1;
                Ok(Token::Eol)
            }
            '\n' => {
                self.line += 1;
                Ok(Token::Eol)
            }
            '=' => Ok(Token::Equal),
            '"' => {
                loop {
                    match self
                        .chars
                        .next()
                    {
                        None | Some('\n') => return Err(self.error("missing closing `\"'")),
                        Some('"') => break,
                        Some(c) => self
                            .buf
                            .push(c),
                    }
                }
                Ok(Token::Quoted)
            }
            _ => {
                if !c.is_ascii_alphabetic() && c != '_' {
                    return Ok(Token::Other);
                }
                self.buf
                    .push(c);
                while let Some(&c) = self
                    .chars
                    .peek()
                {
                    if !c.is_ascii_alphanumeric() && c != '_' {
                        break;
                    }
                    self.buf
                        .push(c);
                    self.chars
                        .next();
                }
                Ok(
                    match self
                        .buf
                        .as_str()
                    {
                        "PRUNE_BIND_MOUNTS" => Token::Keyword(Var::PruneBindMounts),
                        "PRUNEFS" => Token::Keyword(Var::Prunefs),
                        "PRUNENAMES" => Token::Keyword(Var::Prunenames),
                        "PRUNEPATHS" => Token::Keyword(Var::Prunepaths),
                        _ => Token::Identifier,
                    },
                )
            }
        }
    }
}

pub fn parse(text: &str, path: &str) -> io::Result<UpdatedbConf> {
    let mut lex = Lexer::new(text, path);
    let mut conf = UpdatedbConf::default();
    let mut defined: Vec<Var> = Vec::new();

    loop {
        let var = match lex.next_token()? {
            Token::Eof => return Ok(conf),
            Token::Eol => continue,
            Token::Keyword(v) => v,
            Token::Identifier => {
                let name = lex
                    .buf
                    .clone();
                return Err(lex.error(&format!("unknown variable: `{name}'")));
            }
            _ => return Err(lex.error("variable name expected")),
        };
        if defined.contains(&var) {
            let name = lex
                .buf
                .clone();
            return Err(lex.error(&format!("variable `{name}' was already defined")));
        }
        defined.push(var);

        if !matches!(lex.next_token()?, Token::Equal) {
            return Err(lex.error("`=' expected after variable name"));
        }
        if !matches!(lex.next_token()?, Token::Quoted) {
            return Err(lex.error("value in quotes expected after `='"));
        }
        let values = lex
            .buf
            .split_whitespace()
            .map(str::to_string);
        match var {
            Var::Prunenames => conf
                .prunenames
                .extend(values),
            Var::Prunepaths => conf
                .prunepaths
                .extend(values),
            Var::PruneBindMounts | Var::Prunefs => {}
        }

        match lex.next_token()? {
            Token::Eol => continue,
            Token::Eof => return Ok(conf),
            _ => return Err(lex.error("unexpected data after variable value")),
        }
    }
}

/// Read `path`; a missing default file is not an error, a missing file the
/// caller named is.
pub fn load(path: &Path, explicit: bool) -> io::Result<Option<UpdatedbConf>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound && !explicit => return Ok(None),
        Err(e) => {
            return Err(io::Error::new(e.kind(), format!("{}: {e}", path.display())));
        }
    };
    parse(
        &text,
        &path
            .display()
            .to_string(),
    )
    .map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn arch_style() {
        let text = concat!(
            "PRUNE_BIND_MOUNTS = \"no\"\n",
            "PRUNEFS = \"9p afs tmpfs bcachefs\"\n",
            "PRUNENAMES = \".git .hg .svn\"\n",
            "PRUNEPATHS = \"/afs /media /var/tmp\"\n",
        );
        let conf = parse(text, "updatedb.conf").unwrap();
        assert_eq!(conf.prunenames, strs(&[".git", ".hg", ".svn"]));
        assert_eq!(conf.prunepaths, strs(&["/afs", "/media", "/var/tmp"]));
    }

    #[test]
    fn debian_style_with_comments() {
        let text = concat!(
            "# /etc/updatedb.conf: config file for updatedb(8)\n",
            "\n",
            "PRUNE_BIND_MOUNTS=\"yes\"\n",
            "# PRUNENAMES=\".not .this\"\n",
            "PRUNENAMES=\".git .bzr\"   # trailing comment\n",
            "PRUNEPATHS=\"/tmp /var/spool /media\"\n",
        );
        let conf = parse(text, "updatedb.conf").unwrap();
        assert_eq!(conf.prunenames, strs(&[".git", ".bzr"]));
        assert_eq!(conf.prunepaths, strs(&["/tmp", "/var/spool", "/media"]));
    }

    #[test]
    fn empty_file() {
        assert_eq!(parse("", "updatedb.conf").unwrap(), UpdatedbConf::default());
    }

    #[test]
    fn no_space_form() {
        let conf = parse("PRUNENAMES=\"a b\"\n", "updatedb.conf").unwrap();
        assert_eq!(conf.prunenames, strs(&["a", "b"]));
    }

    #[test]
    fn unknown_variable() {
        let err = parse("PRUNEDIRS = \"/tmp\"\n", "updatedb.conf").unwrap_err();
        assert_eq!(
            err.to_string(),
            "updatedb.conf:1: unknown variable: `PRUNEDIRS'"
        );
    }

    #[test]
    fn duplicate_variable() {
        let err = parse("PRUNENAMES = \"a\"\nPRUNENAMES = \"b\"\n", "updatedb.conf").unwrap_err();
        assert_eq!(
            err.to_string(),
            "updatedb.conf:2: variable `PRUNENAMES' was already defined"
        );
    }

    #[test]
    fn missing_closing_quote() {
        let err = parse("PRUNEPATHS = \"/tmp\n", "updatedb.conf").unwrap_err();
        assert_eq!(err.to_string(), "updatedb.conf:1: missing closing `\"'");
    }

    #[test]
    fn trailing_garbage() {
        let err = parse("PRUNEPATHS = \"/tmp\" /var\n", "updatedb.conf").unwrap_err();
        assert_eq!(
            err.to_string(),
            "updatedb.conf:1: unexpected data after variable value"
        );
    }
}
