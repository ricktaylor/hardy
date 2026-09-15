use std::{
    env,
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

use prost::Message;
use prost_types::{
    DescriptorProto, FieldDescriptorProto, FileDescriptorSet,
    field_descriptor_proto::{Label, Type},
};

/// The field names a message is redacted for, whole.
const SECRET_NAMES: &[&str] = &[
    "token",
    "secret",
    "key",
    "password",
    "passphrase",
    "credential",
];

/// The field name endings a message is redacted for.
///
/// A secret the schema names anything else is a secret this build script will
/// not notice, so a new one is named to match: `session_token`, `api_key`,
/// `shared_secret`.
const SECRET_SUFFIXES: &[&str] = &[
    "_token",
    "_secret",
    "_key",
    "_password",
    "_passphrase",
    "_credential",
];

/// The schemas, in the order the generated modules include them.
const SCHEMAS: &[&str] = &[
    "proto/application.proto",
    "proto/service.proto",
    "proto/cla.proto",
    "proto/routing.proto",
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = PathBuf::from(env::var("OUT_DIR")?);

    // The schemas are compiled once for their descriptors, which say which
    // messages carry a secret, and again for the code, which needs that answer
    // to skip the derived `Debug` of each.
    let descriptors = describe(&out)?;
    let redacted = descriptors
        .file
        .iter()
        .flat_map(|file| {
            let package = file.package().to_string();
            check_nested(file);
            file.message_type
                .iter()
                .filter(|message| carries_secret(message))
                .map(move |message| (package.clone(), message.clone()))
        })
        .collect::<Vec<_>>();

    // The shared package is generated on its own, because the pass below maps
    // `hardy.common.v1` to the module that includes it and so emits nothing for
    // it.
    tonic_prost_build::configure()
        .bytes(".")
        .compile_protos(&["proto/common.proto"], &["proto"])?;

    // Each API is included in its own module, one level below the crate root, so
    // a generated relative path to another package would escape the crate.
    tonic_prost_build::configure()
        .bytes(".")
        .extern_path(".hardy.common.v1", "crate::common")
        .skip_debug(
            redacted
                .iter()
                .map(|(package, message)| format!(".{package}.{}", message.name())),
        )
        .compile_protos(SCHEMAS, &["proto"])?;

    write_debug_impls(&out, &redacted)
}

/// Compiles the schemas for their descriptors alone, into a directory whose
/// generated code nothing includes.
fn describe(out: &Path) -> Result<FileDescriptorSet, Box<dyn std::error::Error>> {
    let scratch = out.join("descriptors");
    fs::create_dir_all(&scratch)?;
    let path = scratch.join("api.bin");
    tonic_prost_build::configure()
        .out_dir(&scratch)
        .file_descriptor_set_path(&path)
        .compile_protos(SCHEMAS, &["proto"])?;
    Ok(FileDescriptorSet::decode(&*fs::read(&path)?)?)
}

/// Whether a field of this name holds a secret, and so must never be printed.
fn is_secret(name: &str) -> bool {
    SECRET_NAMES.contains(&name) || SECRET_SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
}

fn carries_secret(message: &DescriptorProto) -> bool {
    message.field.iter().any(|field| is_secret(field.name()))
}

/// Panics if a secret is shaped so that this pass cannot redact it.
///
/// A secret is printed as its length, so it must be a singular `bytes` or
/// `string`, and it must sit directly in a top-level message, since a nested
/// one has no name this pass can write an impl for.
fn check_shape(message: &DescriptorProto, field: &FieldDescriptorProto) {
    let name = field.name();
    assert!(
        matches!(field.r#type(), Type::Bytes | Type::String),
        "{}.{name} is a secret of a type this build script cannot redact by length",
        message.name()
    );
    assert!(
        field.label() != Label::Repeated && !field.proto3_optional(),
        "{}.{name} is a secret this build script cannot redact: it is not a singular field",
        message.name()
    );
}

/// Panics if a nested message carries a secret, which this pass cannot name.
fn check_nested(file: &prost_types::FileDescriptorProto) {
    for message in &file.message_type {
        for nested in &message.nested_type {
            assert!(
                !carries_secret(nested),
                "{}.{} carries a secret and is nested, which this build script cannot redact",
                message.name(),
                nested.name()
            );
        }
    }
}

/// Writes one `<package>.debug.rs` per package, holding the `Debug` the
/// generated code was told to skip: every field as it is, and every secret as
/// its length alone, so a secret cannot reach a log through a message's
/// `Debug`.
fn write_debug_impls(
    out: &Path,
    redacted: &[(String, DescriptorProto)],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut packages: std::collections::BTreeMap<&str, String> = Default::default();
    for (package, message) in redacted {
        let name = message.name();
        let body = packages.entry(package).or_default();
        writeln!(body, "impl core::fmt::Debug for {name} {{")?;
        writeln!(
            body,
            "    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {{"
        )?;
        writeln!(body, "        f.debug_struct(\"{name}\")")?;
        for field in fields_of(message) {
            match field {
                Field::Secret(name) => {
                    let ident = sanitize(&name);
                    writeln!(
                        body,
                        "            .field(\"{name}\", &format_args!(\"{{}} bytes\", \
                         self.{ident}.len()))"
                    )?;
                }
                Field::Plain(name) => {
                    let ident = sanitize(&name);
                    writeln!(body, "            .field(\"{name}\", &self.{ident})")?;
                }
            }
        }
        writeln!(body, "            .finish()")?;
        writeln!(body, "    }}\n}}")?;
    }
    for (package, body) in packages {
        fs::write(out.join(format!("{package}.debug.rs")), body)?;
    }
    Ok(())
}

enum Field {
    /// A field printed as its length alone.
    Secret(String),
    /// A plain field, or the single field a `oneof` becomes.
    Plain(String),
}

/// The fields of `message` as the generated struct has them: the members of a
/// `oneof` are the one field it becomes, in the position of its first member,
/// and a `proto3` `optional`, which is a synthetic `oneof`, stays itself.
fn fields_of(message: &DescriptorProto) -> Vec<Field> {
    let mut fields = Vec::new();
    let mut seen = Vec::new();
    for field in &message.field {
        match field.oneof_index {
            Some(index) if !field.proto3_optional() => {
                if seen.contains(&index) {
                    continue;
                }
                seen.push(index);
                let oneof = &message.oneof_decl[index as usize];
                fields.push(Field::Plain(oneof.name().to_string()));
            }
            _ if is_secret(field.name()) => {
                check_shape(message, field);
                fields.push(Field::Secret(field.name().to_string()));
            }
            _ => fields.push(Field::Plain(field.name().to_string())),
        }
    }
    fields
}

/// The Rust identifier `prost` gives a field of this name.
fn sanitize(name: &str) -> String {
    // Every name in these schemas is already `snake_case`.
    assert!(
        !name.contains(|c: char| c.is_ascii_uppercase()),
        "{name} is not snake_case, which the generated name would differ from"
    );
    if KEYWORDS.contains(&name) {
        format!("r#{name}")
    } else {
        name.to_string()
    }
}

/// The Rust keywords `prost` escapes a field name against.
const KEYWORDS: &[&str] = &[
    "as", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "false", "fn",
    "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref",
    "return", "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where",
    "while",
];
