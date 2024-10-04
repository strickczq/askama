use std::borrow::{Borrow, Cow};
use std::collections::btree_map::{BTreeMap, Entry};
use std::mem::ManuallyDrop;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::{env, fs};

use parser::node::Whitespace;
use parser::{ParseError, Parsed, Syntax, SyntaxBuilder};
use proc_macro2::Span;
#[cfg(feature = "config")]
use serde::Deserialize;

use crate::{CompileError, FileInfo, OnceMap};

#[derive(Debug)]
pub(crate) struct Config {
    pub(crate) dirs: Vec<PathBuf>,
    pub(crate) syntaxes: BTreeMap<String, SyntaxAndCache<'static>>,
    pub(crate) default_syntax: &'static str,
    pub(crate) escapers: Vec<(Vec<Cow<'static, str>>, Cow<'static, str>)>,
    pub(crate) whitespace: WhitespaceHandling,
    // `Config` is self referential and `_key` owns it data, so it must come last
    _key: OwnedConfigKey,
}

impl Drop for Config {
    #[track_caller]
    fn drop(&mut self) {
        panic!();
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
struct OwnedConfigKey(&'static ConfigKey<'static>);

#[derive(Debug, PartialEq, Eq, Hash)]
struct ConfigKey<'a> {
    source: Cow<'a, str>,
    config_path: Option<Cow<'a, str>>,
    template_whitespace: Option<Cow<'a, str>>,
}

impl<'a> ToOwned for ConfigKey<'a> {
    type Owned = OwnedConfigKey;

    fn to_owned(&self) -> Self::Owned {
        let owned_key = ConfigKey {
            source: Cow::Owned(self.source.as_ref().to_owned()),
            config_path: self
                .config_path
                .as_ref()
                .map(|s| Cow::Owned(s.as_ref().to_owned())),
            template_whitespace: self
                .template_whitespace
                .as_ref()
                .map(|s| Cow::Owned(s.as_ref().to_owned())),
        };
        OwnedConfigKey(Box::leak(Box::new(owned_key)))
    }
}

impl<'a> Borrow<ConfigKey<'a>> for OwnedConfigKey {
    #[inline]
    fn borrow(&self) -> &ConfigKey<'a> {
        self.0
    }
}

impl Config {
    pub(crate) fn new(
        source: &str,
        config_path: Option<&str>,
        template_whitespace: Option<&str>,
        config_span: Option<Span>,
    ) -> Result<&'static Config, CompileError> {
        static CACHE: ManuallyDrop<OnceLock<OnceMap<OwnedConfigKey, &'static Config>>> =
            ManuallyDrop::new(OnceLock::new());
        CACHE.get_or_init(OnceMap::default).get_or_try_insert(
            &ConfigKey {
                source: source.into(),
                config_path: config_path.map(Cow::Borrowed),
                template_whitespace: template_whitespace.map(Cow::Borrowed),
            },
            |key| {
                let config = Config::new_uncached(key.to_owned(), config_span)?;
                let config = &*Box::leak(Box::new(config));
                Ok((config._key, config))
            },
            |config| *config,
        )
    }
}

impl Config {
    fn new_uncached(
        key: OwnedConfigKey,
        config_span: Option<Span>,
    ) -> Result<Config, CompileError> {
        let s = key.0.source.as_ref();
        let config_path = key.0.config_path.as_deref();
        let template_whitespace = key.0.template_whitespace.as_deref();

        let root = manifest_root();
        let default_dirs = vec![root.join("templates")];

        let mut syntaxes = BTreeMap::new();
        syntaxes.insert(DEFAULT_SYNTAX_NAME.to_string(), SyntaxAndCache::default());

        let raw = if s.is_empty() {
            RawConfig::default()
        } else {
            RawConfig::from_toml_str(s)?
        };

        let (dirs, default_syntax, mut whitespace) = match raw.general {
            Some(General {
                dirs,
                default_syntax,
                whitespace,
            }) => (
                dirs.map_or(default_dirs, |v| {
                    v.into_iter().map(|dir| root.join(dir)).collect()
                }),
                default_syntax.unwrap_or(DEFAULT_SYNTAX_NAME),
                whitespace,
            ),
            None => (
                default_dirs,
                DEFAULT_SYNTAX_NAME,
                WhitespaceHandling::default(),
            ),
        };
        let file_info = config_path.map(|path| FileInfo::new(Path::new(path), None, None));
        if let Some(template_whitespace) = template_whitespace {
            whitespace = match template_whitespace {
                "suppress" => WhitespaceHandling::Suppress,
                "minimize" => WhitespaceHandling::Minimize,
                "preserve" => WhitespaceHandling::Preserve,
                s => {
                    return Err(CompileError::new(
                        format!("invalid value for `whitespace`: \"{s}\""),
                        file_info,
                    ));
                }
            };
        }

        if let Some(raw_syntaxes) = raw.syntax {
            for raw_s in raw_syntaxes {
                let name = raw_s.name;
                match syntaxes.entry(name.to_string()) {
                    Entry::Vacant(entry) => {
                        entry.insert(raw_s.to_syntax().map(SyntaxAndCache::new).map_err(
                            |err| CompileError::new_with_span(err, file_info, config_span),
                        )?);
                    }
                    Entry::Occupied(_) => {
                        return Err(CompileError::new(
                            format_args!("syntax {name:?} is already defined"),
                            file_info,
                        ));
                    }
                }
            }
        }

        if !syntaxes.contains_key(default_syntax) {
            return Err(CompileError::new(
                format!("default syntax \"{default_syntax}\" not found"),
                file_info,
            ));
        }

        let mut escapers = Vec::new();
        if let Some(configured) = raw.escaper {
            for escaper in configured {
                escapers.push((str_set(&escaper.extensions), escaper.path.into()));
            }
        }
        for (extensions, name) in DEFAULT_ESCAPERS {
            escapers.push((
                str_set(extensions),
                format!("rinja::filters::{name}").into(),
            ));
        }

        Ok(Config {
            dirs,
            syntaxes,
            default_syntax,
            escapers,
            whitespace,
            _key: key,
        })
    }

    pub(crate) fn find_template(
        &self,
        path: &str,
        start_at: Option<&Path>,
        file_info: Option<FileInfo<'_>>,
    ) -> Result<Arc<Path>, CompileError> {
        if let Some(root) = start_at {
            let relative = root.with_file_name(path);
            if relative.exists() {
                return Ok(relative.into());
            }
        }

        for dir in &self.dirs {
            let rooted = dir.join(path);
            if rooted.exists() {
                return Ok(rooted.into());
            }
        }

        Err(CompileError::new(
            format!(
                "template {:?} not found in directories {:?}",
                path, self.dirs
            ),
            file_info,
        ))
    }
}

#[derive(Debug, Default)]
pub(crate) struct SyntaxAndCache<'a> {
    syntax: Syntax<'a>,
    cache: OnceMap<OwnedSyntaxAndCacheKey, Arc<Parsed>>,
}

impl<'a> Deref for SyntaxAndCache<'a> {
    type Target = Syntax<'a>;

    fn deref(&self) -> &Self::Target {
        &self.syntax
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct OwnedSyntaxAndCacheKey(SyntaxAndCacheKey<'static>);

impl Deref for OwnedSyntaxAndCacheKey {
    type Target = SyntaxAndCacheKey<'static>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct SyntaxAndCacheKey<'a> {
    source: Cow<'a, Arc<str>>,
    source_path: Option<Cow<'a, Arc<Path>>>,
}

impl<'a> Borrow<SyntaxAndCacheKey<'a>> for OwnedSyntaxAndCacheKey {
    fn borrow(&self) -> &SyntaxAndCacheKey<'a> {
        &self.0
    }
}

impl<'a> SyntaxAndCache<'a> {
    fn new(syntax: Syntax<'a>) -> Self {
        Self {
            syntax,
            cache: OnceMap::default(),
        }
    }

    pub(crate) fn parse(
        &self,
        source: Arc<str>,
        source_path: Option<Arc<Path>>,
    ) -> Result<Arc<Parsed>, ParseError> {
        self.cache.get_or_try_insert(
            &SyntaxAndCacheKey {
                source: Cow::Owned(source),
                source_path: source_path.map(Cow::Owned),
            },
            |key| {
                let key = OwnedSyntaxAndCacheKey(SyntaxAndCacheKey {
                    source: Cow::Owned(Arc::clone(key.source.as_ref())),
                    source_path: key
                        .source_path
                        .as_deref()
                        .map(|v| Cow::Owned(Arc::clone(v))),
                });
                let parsed = Parsed::new(
                    Arc::clone(key.source.as_ref()),
                    key.source_path.as_deref().map(Arc::clone),
                    &self.syntax,
                )?;
                Ok((key, Arc::new(parsed)))
            },
            Arc::clone,
        )
    }
}

#[cfg_attr(feature = "config", derive(Deserialize))]
#[derive(Default)]
struct RawConfig<'a> {
    #[cfg_attr(feature = "config", serde(borrow))]
    general: Option<General<'a>>,
    syntax: Option<Vec<SyntaxBuilder<'a>>>,
    escaper: Option<Vec<RawEscaper<'a>>>,
}

impl RawConfig<'_> {
    #[cfg(feature = "config")]
    fn from_toml_str(s: &str) -> Result<RawConfig<'_>, CompileError> {
        basic_toml::from_str(s).map_err(|e| {
            CompileError::no_file_info(format!("invalid TOML in {CONFIG_FILE_NAME}: {e}"), None)
        })
    }

    #[cfg(not(feature = "config"))]
    fn from_toml_str(_: &str) -> Result<RawConfig<'_>, CompileError> {
        Err(CompileError::no_file_info(
            "TOML support not available",
            None,
        ))
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug, Hash)]
#[cfg_attr(feature = "config", derive(Deserialize))]
#[cfg_attr(feature = "config", serde(field_identifier, rename_all = "lowercase"))]
pub(crate) enum WhitespaceHandling {
    /// The default behavior. It will leave the whitespace characters "as is".
    #[default]
    Preserve,
    /// It'll remove all the whitespace characters before and after the jinja block.
    Suppress,
    /// It'll remove all the whitespace characters except one before and after the jinja blocks.
    /// If there is a newline character, the preserved character in the trimmed characters, it will
    /// the one preserved.
    Minimize,
}

impl From<WhitespaceHandling> for Whitespace {
    fn from(ws: WhitespaceHandling) -> Self {
        match ws {
            WhitespaceHandling::Suppress => Whitespace::Suppress,
            WhitespaceHandling::Preserve => Whitespace::Preserve,
            WhitespaceHandling::Minimize => Whitespace::Minimize,
        }
    }
}

#[cfg_attr(feature = "config", derive(Deserialize))]
struct General<'a> {
    #[cfg_attr(feature = "config", serde(borrow))]
    dirs: Option<Vec<&'a str>>,
    default_syntax: Option<&'a str>,
    #[cfg_attr(feature = "config", serde(default))]
    whitespace: WhitespaceHandling,
}

#[cfg_attr(feature = "config", derive(Deserialize))]
struct RawEscaper<'a> {
    path: &'a str,
    extensions: Vec<&'a str>,
}

pub(crate) fn read_config_file(
    config_path: Option<&str>,
    span: Option<Span>,
) -> Result<String, CompileError> {
    let root = manifest_root();
    let filename = match config_path {
        Some(config_path) => root.join(config_path),
        None => root.join(CONFIG_FILE_NAME),
    };

    if filename.exists() {
        fs::read_to_string(&filename).map_err(|err| {
            CompileError::no_file_info(
                format!("unable to read {}: {err}", filename.display()),
                span,
            )
        })
    } else if config_path.is_some() {
        Err(CompileError::no_file_info(
            format!("`{}` does not exist", filename.display()),
            span,
        ))
    } else {
        Ok(String::new())
    }
}

fn manifest_root() -> PathBuf {
    env::var_os("CARGO_MANIFEST_DIR").map_or_else(|| PathBuf::from("."), PathBuf::from)
}

fn str_set(vals: &[&'static str]) -> Vec<Cow<'static, str>> {
    vals.iter().map(|s| Cow::Borrowed(*s)).collect()
}

static CONFIG_FILE_NAME: &str = "rinja.toml";
static DEFAULT_SYNTAX_NAME: &str = "default";
static DEFAULT_ESCAPERS: &[(&[&str], &str)] = &[
    (
        &[
            "html", "htm", "j2", "jinja", "jinja2", "rinja", "svg", "xml",
        ],
        "Html",
    ),
    (&["md", "none", "txt", "yml", ""], "Text"),
];

#[cfg(test)]
mod tests {
    use std::env;
    use std::path::{Path, PathBuf};

    use super::*;

    #[test]
    fn test_default_config() {
        let mut root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
        root.push("templates");
        let config = Config::new("", None, None, None).unwrap();
        assert_eq!(config.dirs, vec![root]);
    }

    #[cfg(feature = "config")]
    #[test]
    fn test_config_dirs() {
        let mut root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
        root.push("tpl");
        let config = Config::new("[general]\ndirs = [\"tpl\"]", None, None, None).unwrap();
        assert_eq!(config.dirs, vec![root]);
    }

    fn assert_eq_rooted(actual: &Path, expected: &str) {
        let mut root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
        root.push("templates");
        let mut inner = PathBuf::new();
        inner.push(expected);
        assert_eq!(actual.strip_prefix(root).unwrap(), inner);
    }

    #[test]
    fn find_absolute() {
        let config = Config::new("", None, None, None).unwrap();
        let root = config.find_template("a.html", None, None).unwrap();
        let path = config
            .find_template("sub/b.html", Some(&root), None)
            .unwrap();
        assert_eq_rooted(&path, "sub/b.html");
    }

    #[test]
    #[should_panic]
    fn find_relative_nonexistent() {
        let config = Config::new("", None, None, None).unwrap();
        let root = config.find_template("a.html", None, None).unwrap();
        config.find_template("c.html", Some(&root), None).unwrap();
    }

    #[test]
    fn find_relative() {
        let config = Config::new("", None, None, None).unwrap();
        let root = config.find_template("sub/b.html", None, None).unwrap();
        let path = config.find_template("c.html", Some(&root), None).unwrap();
        assert_eq_rooted(&path, "sub/c.html");
    }

    #[test]
    fn find_relative_sub() {
        let config = Config::new("", None, None, None).unwrap();
        let root = config.find_template("sub/b.html", None, None).unwrap();
        let path = config
            .find_template("sub1/d.html", Some(&root), None)
            .unwrap();
        assert_eq_rooted(&path, "sub/sub1/d.html");
    }

    #[cfg(feature = "config")]
    #[test]
    fn add_syntax() {
        let raw_config = r#"
        [general]
        default_syntax = "foo"

        [[syntax]]
        name = "foo"
        block_start = "{<"

        [[syntax]]
        name = "bar"
        expr_start = "{!"
        "#;

        let default_syntax = Syntax::default();
        let config = Config::new(raw_config, None, None, None).unwrap();
        assert_eq!(config.default_syntax, "foo");

        let foo = config.syntaxes.get("foo").unwrap();
        assert_eq!(foo.block_start, "{<");
        assert_eq!(foo.block_end, default_syntax.block_end);
        assert_eq!(foo.expr_start, default_syntax.expr_start);
        assert_eq!(foo.expr_end, default_syntax.expr_end);
        assert_eq!(foo.comment_start, default_syntax.comment_start);
        assert_eq!(foo.comment_end, default_syntax.comment_end);

        let bar = config.syntaxes.get("bar").unwrap();
        assert_eq!(bar.block_start, default_syntax.block_start);
        assert_eq!(bar.block_end, default_syntax.block_end);
        assert_eq!(bar.expr_start, "{!");
        assert_eq!(bar.expr_end, default_syntax.expr_end);
        assert_eq!(bar.comment_start, default_syntax.comment_start);
        assert_eq!(bar.comment_end, default_syntax.comment_end);
    }

    #[cfg(feature = "config")]
    #[test]
    fn add_syntax_two() {
        let raw_config = r#"
        syntax = [{ name = "foo", block_start = "{<" },
                  { name = "bar", expr_start = "{!" } ]

        [general]
        default_syntax = "foo"
        "#;

        let default_syntax = Syntax::default();
        let config = Config::new(raw_config, None, None, None).unwrap();
        assert_eq!(config.default_syntax, "foo");

        let foo = config.syntaxes.get("foo").unwrap();
        assert_eq!(foo.block_start, "{<");
        assert_eq!(foo.block_end, default_syntax.block_end);
        assert_eq!(foo.expr_start, default_syntax.expr_start);
        assert_eq!(foo.expr_end, default_syntax.expr_end);
        assert_eq!(foo.comment_start, default_syntax.comment_start);
        assert_eq!(foo.comment_end, default_syntax.comment_end);

        let bar = config.syntaxes.get("bar").unwrap();
        assert_eq!(bar.block_start, default_syntax.block_start);
        assert_eq!(bar.block_end, default_syntax.block_end);
        assert_eq!(bar.expr_start, "{!");
        assert_eq!(bar.expr_end, default_syntax.expr_end);
        assert_eq!(bar.comment_start, default_syntax.comment_start);
        assert_eq!(bar.comment_end, default_syntax.comment_end);
    }

    #[cfg(feature = "config")]
    #[test]
    fn longer_delimiters() {
        let raw_config = r#"
        [[syntax]]
        name = "emoji"
        block_start = "👉🙂👉"
        block_end = "👈🙃👈"
        expr_start = "🤜🤜"
        expr_end = "🤛🤛"
        comment_start = "👎_(ツ)_👎"
        comment_end = "👍:D👍"

        [general]
        default_syntax = "emoji"
        "#;

        let config = Config::new(raw_config, None, None, None).unwrap();
        assert_eq!(config.default_syntax, "emoji");

        let foo = config.syntaxes.get("emoji").unwrap();
        assert_eq!(foo.block_start, "👉🙂👉");
        assert_eq!(foo.block_end, "👈🙃👈");
        assert_eq!(foo.expr_start, "🤜🤜");
        assert_eq!(foo.expr_end, "🤛🤛");
        assert_eq!(foo.comment_start, "👎_(ツ)_👎");
        assert_eq!(foo.comment_end, "👍:D👍");
    }

    #[cfg(feature = "config")]
    #[test]
    fn illegal_delimiters() {
        #[track_caller]
        fn expect_err<T, E>(result: Result<T, E>) -> E {
            match result {
                Ok(_) => panic!("should have failed"),
                Err(err) => err,
            }
        }

        let raw_config = r#"
        [[syntax]]
        name = "too_short"
        block_start = "<"
        "#;
        let config = Config::new(raw_config, None, None, None);
        assert_eq!(
            expect_err(config).msg,
            r#"delimiters must be at least two characters long. The opening block delimiter ("<") is too short"#,
        );

        let raw_config = r#"
        [[syntax]]
        name = "contains_ws"
        block_start = " {{ "
        "#;
        let config = Config::new(raw_config, None, None, None);
        assert_eq!(
            expect_err(config).msg,
            r#"delimiters may not contain white spaces. The opening block delimiter (" {{ ") contains white spaces"#,
        );

        let raw_config = r#"
        [[syntax]]
        name = "is_prefix"
        block_start = "{{"
        expr_start = "{{$"
        comment_start = "{{#"
        "#;
        let config = Config::new(raw_config, None, None, None);
        assert_eq!(
            expect_err(config).msg,
            r#"an opening delimiter may not be the prefix of another delimiter. The block delimiter ("{{") clashes with the expression delimiter ("{{$")"#,
        );
    }

    #[cfg(feature = "config")]
    #[should_panic]
    #[test]
    fn use_default_at_syntax_name() {
        let raw_config = r#"
        syntax = [{ name = "default" }]
        "#;

        let _config = Config::new(raw_config, None, None, None).unwrap();
    }

    #[cfg(feature = "config")]
    #[should_panic]
    #[test]
    fn duplicated_syntax_name_on_list() {
        let raw_config = r#"
        syntax = [{ name = "foo", block_start = "~<" },
                  { name = "foo", block_start = "%%" } ]
        "#;

        let _config = Config::new(raw_config, None, None, None).unwrap();
    }

    #[cfg(feature = "config")]
    #[should_panic]
    #[test]
    fn is_not_exist_default_syntax() {
        let raw_config = r#"
        [general]
        default_syntax = "foo"
        "#;

        let _config = Config::new(raw_config, None, None, None).unwrap();
    }

    #[cfg(feature = "config")]
    #[test]
    fn escape_modes() {
        let config = Config::new(
            r#"
            [[escaper]]
            path = "::my_filters::Js"
            extensions = ["js"]
        "#,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(config.escapers, vec![
            (str_set(&["js"]), "::my_filters::Js".into()),
            (
                str_set(&[
                    "html", "htm", "j2", "jinja", "jinja2", "rinja", "svg", "xml"
                ]),
                "rinja::filters::Html".into()
            ),
            (
                str_set(&["md", "none", "txt", "yml", ""]),
                "rinja::filters::Text".into()
            ),
        ]);
    }

    #[cfg(feature = "config")]
    #[test]
    fn test_whitespace_parsing() {
        let config = Config::new(
            r#"
            [general]
            whitespace = "suppress"
            "#,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(config.whitespace, WhitespaceHandling::Suppress);

        let config = Config::new(r#""#, None, None, None).unwrap();
        assert_eq!(config.whitespace, WhitespaceHandling::Preserve);

        let config = Config::new(
            r#"
            [general]
            whitespace = "preserve"
            "#,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(config.whitespace, WhitespaceHandling::Preserve);

        let config = Config::new(
            r#"
            [general]
            whitespace = "minimize"
            "#,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(config.whitespace, WhitespaceHandling::Minimize);
    }

    #[cfg(feature = "config")]
    #[test]
    fn test_whitespace_in_template() {
        // Checking that template arguments have precedence over general configuration.
        // So in here, in the template arguments, there is `whitespace = "minimize"` so
        // the `WhitespaceHandling` should be `Minimize` as well.
        let config = Config::new(
            r#"
            [general]
            whitespace = "suppress"
            "#,
            None,
            Some("minimize"),
            None,
        )
        .unwrap();
        assert_eq!(config.whitespace, WhitespaceHandling::Minimize);

        let config = Config::new(r#""#, None, Some("minimize"), None).unwrap();
        assert_eq!(config.whitespace, WhitespaceHandling::Minimize);
    }

    #[test]
    fn test_config_whitespace_error() {
        let config = Config::new(r"", None, Some("trim"), None);
        if let Err(err) = config {
            assert_eq!(err.msg, "invalid value for `whitespace`: \"trim\"");
        } else {
            panic!("Config::new should have return an error");
        }
    }
}
