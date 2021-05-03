// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#[allow(unused)]
mod apple_certificates;
#[allow(unused)]
mod bundle_signing;
#[allow(unused)]
mod certificate;
#[allow(unused)]
mod code_directory;
#[allow(unused)]
mod code_hash;
#[allow(unused)]
mod code_requirement;
#[allow(unused)]
mod code_resources;
mod error;
#[allow(unused)]
mod macho;
#[allow(unused)]
mod macho_signing;
#[allow(non_upper_case_globals, unused)]
#[cfg(target_os = "macos")]
mod macos;
#[allow(unused)]
mod policy;
#[allow(unused)]
mod signing;
#[allow(unused)]
mod specification;
#[allow(unused)]
mod verify;

use {
    crate::{
        bundle_signing::BundleSigner,
        certificate::{
            create_self_signed_code_signing_certificate, parse_pfx_data, CertificateProfile,
        },
        code_directory::{CodeDirectoryBlob, CodeSignatureFlags, ExecutableSegmentFlags},
        code_hash::compute_code_hashes,
        code_requirement::CodeRequirements,
        error::AppleCodesignError,
        macho::{
            find_signature_data, parse_signature_data, Blob, CodeSigningSlot, DigestType,
            RequirementSetBlob,
        },
        macho_signing::MachOSigner,
        signing::{SettingsScope, SigningSettings},
    },
    clap::{App, AppSettings, Arg, ArgMatches, SubCommand},
    cryptographic_message_syntax::SignedData,
    goblin::mach::{Mach, MachO},
    slog::{error, o, warn, Drain},
    std::{convert::TryFrom, io::Write, path::PathBuf, str::FromStr},
    x509_certificate::{CapturedX509Certificate, EcdsaCurve, InMemorySigningKeyPair, KeyAlgorithm},
};

#[cfg(target_os = "macos")]
use crate::macos::{macos_keychain_find_certificate_chain, KeychainDomain};

const EXTRACT_ABOUT: &str = "\
Extract code signature data from a Mach-O binary.

Given the path to a Mach-O binary (including fat/universal) binaries, this
command will parse and print requested data to stdout.

The --data argument controls which data to extract and how to print it.
Possible values are:

blobs
   Low-level information on the records in the embedded code signature.
cms-pem
   Like cms-raw except it prints PEM encoded data, which is ASCII and
   safe to print to terminals.
cms-raw
   Print the payload of the CMS blob. This should be well-formed BER
   encoded ASN.1 data. (This will print binary to stdout.)
cms
   Print the ASN.1 decoded CMS data.
code-directory-raw
   Raw binary data composing the code directory data structure.
code-directory
   Information on the main code directory data structure.
code-directory-serialized
   Reserialize the parsed code directory, parse it again, and then print
   it like `code-directory` would.
code-directory-serialized-raw
   Reserialize the parsed code directory and emit its binary. Useful
   for comparing round-tripping of code directory data.
linkededit-segment-raw
   Complete content of the __LINKEDIT Mach-O segment as binary.
macho-load-commands
   Print information about mach-o load commands in the binary.
requirements-raw
   Raw binary data composing the requirements blob/slot.
requirements
   Parsed code requirement statement/expression.
requirements-rust
   Dump the internal Rust data structures representing the requirements
   expressions.
requirements-serialized
   Reserialize the code requirements blob, parse it again, and then
   print it like `requirements` would.
requirements-serialized-raw
   Reserialize the code requirements blob and emit its binary.
signature-raw
   Raw binary data composing the signature data embedded in the binary.
segment-info
   Information about Mach-O segments in the binary and where the
   __LINKEDIT is in relationship to the binary.
superblob
   The SuperBlob record and high-level details of embedded Blob
   records, including digests of every Blob.
";

const GENERATE_SELF_SIGNED_CERTIFICATE_ABOUT: &str = "\
Generate a self-signed certificate that can be used for code signing.

This command will generate a new key pair using the algorithm of choice
then create an X.509 certificate wrapper for it that is signed with the
just-generated private key. The created X.509 certificate has extensions
that mark it as appropriate for code signing.

Certificates generated with this command can be useful for local testing.
However, because it is a self-signed certificate and isn't signed by a
trusted certificate authority, Apple operating systems may refuse to
load binaries signed with it.

The command prints 2 PEM encoded blocks. One block is for the X.509 public
certificate. The other is for the PKCS#8 private key (which can include
the public key).
";

const PARSE_CODE_SIGNING_REQUIREMENT_ABOUT: &str = "\
Parse code signing requirement data into human readable text.

This command can be used to parse binary code signing requirement data and
print it in various formats.

The source input format is the binary code requirement serialization. This
is the format generated by Apple's `csreq` tool via `csreq -b`. The binary
data begins with header magic `0xfade0c00`.

The default output format is the Code Signing Requirement Language. But the
output format can be changed via the --format argument.

Our Code Signing Requirement Language output may differ from Apple's. For
example, `and` and `or` expressions always have their sub-expressions surrounded
by parentheses (e.g. `(a) and (b)` instead of `a and b`) and strings are always
quoted. The differences, however, should not matter to the parser or result
in a different binary serialization.
";

const SIGN_ABOUT: &str = "\
Adds code signatures to a signable entity.

This command can sign the following entities:

* A single Mach-O binary (specified by its file path)
* A bundle (specified by its directory path)

If the input is Mach-O binary, it can be a single or multiple/fat/universal
Mach-O binary. If a fat binary is given, each Mach-O within that binary will
be signed.

If the input is a bundle, the bundle will be recursively signed. If the
bundle contains nested bundles or Mach-O binaries, those will be signed
automatically.

# Settings Scope

The following signing settings are global and apply to all signed entities:

* --digest
* --pem-source
* --team-name
* --timestamp-url

The following signing settings can be scoped so they only apply to certain
entities:

* --binary-identifier
* --code-requirements-path
* --code-resources-path
* --code-signature-flags
* --entitlements-xml-path
* --executable-segment-flags
* --info-plist-path

Scoped settings take the form <value> or <scope>:<value>. If the 2nd form
is used, the string before the first colon is parsed as a \"scoping string\".
It can have the following values:

* `main` - Applies to the main entity being signed and all nested entities.
* `@<integer>` - e.g. `@0`. Applies to a Mach-O within a fat binary at the
  specified index. 0 means the first Mach-O in a fat binary.
* `@[cpu_type=<int>` - e.g. `@[cpu_type=7]`. Applies to a Mach-O within a fat
  binary targeting a numbered CPU architecture (using numeric constants
  as defined by Mach-O).
* `@[cpu_type=<string>` - e.g. `@[cpu_type=x86_64]`. Applies to a Mach-O within
  a fat binary targeting a CPU architecture identified by a string. See below
  for the list of recognized values.
* `<string>` - e.g. `path/to/file`. Applies to content at a given path. This
  should be the bundle-relative path to a Mach-O binary, a nested bundle, or
  a Mach-O binary within a nested bundle. If a nested bundle is referenced,
  settings apply to everything within that bundle.
* `<string>@<int>` - e.g. `path/to/file@0`. Applies to a Mach-O within a
  fat binary at the given path. If the path is to a bundle, the setting applies
  to all Mach-O binaries in that bundle.
* `<string>@[cpu_type=<int|string>]` e.g. `Contents/MacOS/binary@[cpu_type=7]`
  or `Contents/MacOS/binary@[cpu_type=arm64]`. Applies to a Mach-O within a
  fat binary targeting a CPU architecture identified by its integer constant
  or string name. If the path is to a bundle, the setting applies to all
  Mach-O binaries in that bundle.

The following named CPU architectures are recognized:

* arm
* arm64
* arm64_32
* x86_64

Signing will traverse into nested entities:

* A fat Mach-O binary will traverse into the multiple Mach-O binaries within.
* A bundle will traverse into nested bundles.
* A bundle will traverse non-code \"resource\" files and sign their digests.
* A bundle will traverse non-main Mach-O binaries and sign them, adding their
  metadata to the signed resources file.

# Bundle Signing Overrides Settings

When signing bundles, some settings specified on the command line will be
ignored. This is to ensure that the produced signing data is correct. The
settings ignored include (but may not be limited to):

* --binary-identifier for the main executable. The `CFBundleIdentifier` value
  from the bundle's `Info.plist` will be used instead.
* --code-resources-path. The code resources data will be computed automatically
  as part of signing the bundle.
* --info-plist-path. The `Info.plist` from the bundle will be used instead.

# Designated Code Requirements

When using Apple issued code signing certificates, we will attempt to apply
an appropriate designated requirement automatically during signing which
matches the behavior of what `codesign` would do. We do not yet support all
signing certificates and signing targets for this, however. So you may
need to provide your own requirements. 

Designated code requirements can be specified via --code-requirements-path.

This file MUST contain a binary/compiled code requirements expression. We do
not (yet) support parsing the human-friendly code requirements DSL. A
binary/compiled file can be produced via Apple's `csreq` tool. e.g.
`csreq -r '=<expression>' -b /output/path`. If code requirements data is
specified, it will be parsed and displayed as part of signing to ensure it
is well-formed.

# Code Signing Key Pair

By default, the embedded code signature will only contain digests of the
binary and other important entities (such as entitlements and resources).
This is often referred to as \"ad-hoc\" signing.

To use a code signing key/certificate to derive a cryptographic signature,
you must specify a source certificate to use. This can be done in the following
ways:

* The --pfx-file denotes the location to a PFX formatted file. These are
  often .pfx or .p12 files. If you use PFX files, remember to specify
  --pfx-password or --pfx-password-path so an appropriate password is
  used to read the PFX file.
* The --pem-source argument defines paths to files containing PEM encoded
  certificate/key data. (e.g. files with \"===== BEGIN CERTIFICATE =====\").

If you export a code signing certificate from the macOS keychain via the
`Keychain Access` application as a .p12 file, we should be able to read these
files via --pfx-file.

When using --pem-source, certificates and public keys are parsed from
`BEGIN CERTIFICATE` and `BEGIN PRIVATE KEY` sections in the files.

The way certificate discovery works is that --pfx-file is read followed by
all values to --pem-source. The seen signing keys and certificates are
collected. After collection, there must be 0 or 1 signing keys present, or
an error occurs. The first encountered public certificate is assigned
to be paired with the signing key. All remaining certificates are assumed
to constitute the CA issuing chain and will be added to the signature
data to facilitate validation.

If you are using an Apple-issued code signing certificate, we detect this
and automatically register the Apple CA certificate chain so it is included
in the digital signature. This matches the behavior of the `codesign` tool.

For best results, put your private key and its corresponding X.509 certificate
in a single file, either a PFX or PEM formatted file. Then add any additional
certificates constituting the signing chain in a separate PEM file.

When using a code signing key/certificate, a Time-Stamp Protocol server URL
can be specified via --timestamp-url. By default, Apple's server is used. The
special value \"none\" can disable using a timestamp server.
";

#[allow(unused)]
const APPLE_TIMESTAMP_URL: &str = "http://timestamp.apple.com/ts01";

const SUPPORTED_HASHES: &[&str; 6] = &[
    "none",
    "sha1",
    "sha256",
    "sha256-truncated",
    "sha384",
    "sha512",
];

fn get_logger() -> slog::Logger {
    let decorator = slog_term::TermDecorator::new().build();
    let drain = slog_term::CompactFormat::new(decorator).build();
    let drain = std::sync::Mutex::new(drain).fuse();

    slog::Logger::root(drain, o!())
}

fn parse_scoped_value(s: &str) -> Result<(SettingsScope, &str), AppleCodesignError> {
    let parts = s.splitn(2, ':').collect::<Vec<_>>();

    match parts.len() {
        1 => Ok((SettingsScope::Main, s)),
        2 => Ok((SettingsScope::try_from(parts[0])?, parts[1])),
        _ => Err(AppleCodesignError::CliBadArgument),
    }
}

fn get_macho_from_data(data: &[u8], universal_index: usize) -> Result<MachO, AppleCodesignError> {
    let mach = Mach::parse(data)?;

    match mach {
        Mach::Binary(macho) => Ok(macho),
        Mach::Fat(multiarch) => {
            eprintln!(
                "found fat/universal Mach-O binary with {} architectures; examining binary at index {}",
                multiarch.narches, universal_index
            );

            Ok(multiarch.get(universal_index)?)
        }
    }
}

fn command_compute_code_hashes(args: &ArgMatches) -> Result<(), AppleCodesignError> {
    let path = args
        .value_of("path")
        .ok_or(AppleCodesignError::CliBadArgument)?;
    let index = args.value_of("universal_index").unwrap();
    let index = usize::from_str(index).map_err(|_| AppleCodesignError::CliBadArgument)?;
    let hash_type = DigestType::try_from(args.value_of("hash").unwrap())?;
    let page_size = if let Some(page_size) = args.value_of("page_size") {
        Some(usize::from_str(page_size).map_err(|_| AppleCodesignError::CliBadArgument)?)
    } else {
        None
    };

    let data = std::fs::read(path)?;
    let macho = get_macho_from_data(&data, index)?;

    let hashes = compute_code_hashes(&macho, hash_type, page_size)?;

    for hash in hashes {
        println!("{}", hex::encode(hash));
    }

    Ok(())
}

fn command_extract(args: &ArgMatches) -> Result<(), AppleCodesignError> {
    let path = args
        .value_of("path")
        .ok_or(AppleCodesignError::CliBadArgument)?;
    let format = args
        .value_of("data")
        .ok_or(AppleCodesignError::CliBadArgument)?;
    let index = args.value_of("universal_index").unwrap();
    let index = usize::from_str(index).map_err(|_| AppleCodesignError::CliBadArgument)?;

    let data = std::fs::read(path)?;

    let macho = get_macho_from_data(&data, index)?;

    let sig = find_signature_data(&macho)?.ok_or(AppleCodesignError::BinaryNoCodeSignature)?;

    match format {
        "blobs" => {
            let embedded = parse_signature_data(&sig.signature_data)?;

            for blob in embedded.blobs {
                let parsed = blob.into_parsed_blob()?;
                println!("{:#?}", parsed);
            }
        }
        "cms-pem" => {
            let embedded = parse_signature_data(&sig.signature_data)?;
            if let Some(cms) = embedded.signature_data()? {
                print!(
                    "{}",
                    pem::encode(&pem::Pem {
                        tag: "PKCS7".to_string(),
                        contents: cms.to_vec(),
                    })
                );
            } else {
                eprintln!("no CMS data");
            }
        }
        "cms-raw" => {
            let embedded = parse_signature_data(&sig.signature_data)?;
            if let Some(cms) = embedded.signature_data()? {
                std::io::stdout().write_all(cms)?;
            } else {
                eprintln!("no CMS data");
            }
        }
        "cms" => {
            let embedded = parse_signature_data(&sig.signature_data)?;
            if let Some(cms) = embedded.signature_data()? {
                let signed_data = SignedData::parse_ber(cms)?;

                println!("{:#?}", signed_data);
            } else {
                eprintln!("no CMS data");
            }
        }
        "code-directory-raw" => {
            let embedded = parse_signature_data(&sig.signature_data)?;

            if let Some(blob) = embedded.find_slot(CodeSigningSlot::CodeDirectory) {
                std::io::stdout().write_all(blob.data)?;
            } else {
                eprintln!("no code directory");
            }
        }
        "code-directory-serialized-raw" => {
            let embedded = parse_signature_data(&sig.signature_data)?;

            if let Ok(Some(cd)) = embedded.code_directory() {
                std::io::stdout().write_all(&cd.to_blob_bytes()?)?;
            } else {
                eprintln!("no code directory");
            }
        }
        "code-directory-serialized" => {
            let embedded = parse_signature_data(&sig.signature_data)?;

            if let Ok(Some(cd)) = embedded.code_directory() {
                let serialized = cd.to_blob_bytes()?;
                println!("{:#?}", CodeDirectoryBlob::from_blob_bytes(&serialized)?);
            }
        }
        "code-directory" => {
            let embedded = parse_signature_data(&sig.signature_data)?;

            if let Some(cd) = embedded.code_directory()? {
                println!("{:#?}", cd);
            } else {
                eprintln!("no code directory");
            }
        }
        "linkedit-segment-raw" => {
            std::io::stdout().write_all(sig.linkedit_segment_data)?;
        }
        "macho-load-commands" => {
            println!("load command count: {}", macho.load_commands.len());

            for command in &macho.load_commands {
                println!(
                    "{}; offsets=0x{:x}-0x{:x} ({}-{}); size={}",
                    goblin::mach::load_command::cmd_to_str(command.command.cmd()),
                    command.offset,
                    command.offset + command.command.cmdsize(),
                    command.offset,
                    command.offset + command.command.cmdsize(),
                    command.command.cmdsize(),
                );
            }
        }
        "requirements-raw" => {
            let embedded = parse_signature_data(&sig.signature_data)?;

            if let Some(blob) = embedded.find_slot(CodeSigningSlot::RequirementSet) {
                std::io::stdout().write_all(blob.data)?;
            } else {
                eprintln!("no requirements");
            }
        }
        "requirements-rust" => {
            let embedded = parse_signature_data(&sig.signature_data)?;

            if let Some(reqs) = embedded.code_requirements()? {
                for (typ, req) in &reqs.requirements {
                    for expr in req.parse_expressions()?.iter() {
                        println!("{} => {:#?}", typ, expr);
                    }
                }
            } else {
                eprintln!("no requirements");
            }
        }
        "requirements-serialized-raw" => {
            let embedded = parse_signature_data(&sig.signature_data)?;

            if let Some(reqs) = embedded.code_requirements()? {
                std::io::stdout().write_all(&reqs.to_blob_bytes()?)?;
            } else {
                eprintln!("no requirements");
            }
        }
        "requirements-serialized" => {
            let embedded = parse_signature_data(&sig.signature_data)?;

            if let Some(reqs) = embedded.code_requirements()? {
                let serialized = reqs.to_blob_bytes()?;
                println!("{:#?}", RequirementSetBlob::from_blob_bytes(&serialized)?);
            } else {
                eprintln!("no requirements");
            }
        }
        "requirements" => {
            let embedded = parse_signature_data(&sig.signature_data)?;

            if let Some(reqs) = embedded.code_requirements()? {
                for (typ, req) in &reqs.requirements {
                    for expr in req.parse_expressions()?.iter() {
                        println!("{} => {}", typ, expr);
                    }
                }
            } else {
                eprintln!("no requirements");
            }
        }
        "segment-info" => {
            println!("segments count: {}", sig.segments_count);
            for (i, segment) in macho.segments.iter().enumerate() {
                println!(
                    "segment #{}; {}; start offset {}; end offset {}; size {}",
                    i,
                    segment.name()?,
                    segment.fileoff,
                    segment.fileoff as usize + segment.data.len(),
                    segment.data.len()
                );
            }
            println!("__LINKEDIT segment index: {}", sig.linkedit_segment_index);
            println!(
                "__LINKEDIT segment start offset: {}",
                sig.linkedit_segment_start_offset
            );
            println!(
                "__LINKEDIT segment end offset: {}",
                sig.linkedit_segment_end_offset
            );
            println!(
                "__LINKEDIT segment size: {}",
                sig.linkedit_segment_data.len()
            );
            println!(
                "__LINKEDIT signature global start offset: {}",
                sig.linkedit_signature_start_offset
            );
            println!(
                "__LINKEDIT signature global end offset: {}",
                sig.linkedit_signature_end_offset
            );
            println!(
                "__LINKEDIT signature local segment start offset: {}",
                sig.signature_start_offset
            );
            println!(
                "__LINKEDIT signature local segment end offset: {}",
                sig.signature_end_offset
            );
            println!("__LINKEDIT signature size: {}", sig.signature_data.len());
        }
        "signature-raw" => {
            std::io::stdout().write_all(&sig.signature_data)?;
        }
        "superblob" => {
            let embedded = parse_signature_data(&sig.signature_data)?;

            println!("file start offset: {}", sig.linkedit_signature_start_offset);
            println!("file end offset: {}", sig.linkedit_signature_end_offset);
            println!("__LINKEDIT start offset: {}", sig.signature_start_offset);
            println!("__LINKEDIT end offset: {}", sig.signature_end_offset);
            println!("length: {}", embedded.length);
            println!("blob count: {}", embedded.count);
            println!("blobs:");
            for blob in embedded.blobs {
                println!("- index: {}", blob.index);
                println!(
                    "  offsets: 0x{:x}-0x{:x} ({}-{})",
                    blob.offset,
                    blob.offset + blob.length - 1,
                    blob.offset,
                    blob.offset + blob.length - 1
                );
                println!("  length: {}", blob.length);
                println!("  slot: {:?}", blob.slot);
                println!("  magic: {:?} (0x{:x})", blob.magic, u32::from(blob.magic));
                println!(
                    "  sha1: {}",
                    hex::encode(blob.digest_with(DigestType::Sha1)?)
                );
                println!(
                    "  sha256: {}",
                    hex::encode(blob.digest_with(DigestType::Sha256)?)
                );
                println!(
                    "  sha256-truncated: {}",
                    hex::encode(blob.digest_with(DigestType::Sha256Truncated)?)
                );
                println!(
                    "  sha384: {}",
                    hex::encode(blob.digest_with(DigestType::Sha384)?),
                );
                println!(
                    "  sha512: {}",
                    hex::encode(blob.digest_with(DigestType::Sha512)?),
                );
                println!(
                    "  sha1-base64: {}",
                    base64::encode(blob.digest_with(DigestType::Sha1)?)
                );
                println!(
                    "  sha256-base64: {}",
                    base64::encode(blob.digest_with(DigestType::Sha256)?)
                );
                println!(
                    "  sha256-truncated-base64: {}",
                    base64::encode(blob.digest_with(DigestType::Sha256Truncated)?)
                );
                println!(
                    "  sha384-base64: {}",
                    base64::encode(blob.digest_with(DigestType::Sha384)?)
                );
                println!(
                    "  sha512-base64: {}",
                    base64::encode(blob.digest_with(DigestType::Sha512)?)
                );
            }
        }
        _ => panic!("unhandled format: {}", format),
    }

    Ok(())
}

fn command_generate_self_signed_certificate(args: &ArgMatches) -> Result<(), AppleCodesignError> {
    let algorithm = match args
        .value_of("algorithm")
        .ok_or(AppleCodesignError::CliBadArgument)?
    {
        "ecdsa" => KeyAlgorithm::Ecdsa(EcdsaCurve::Secp256r1),
        "ed25519" => KeyAlgorithm::Ed25519,
        value => panic!(
            "algorithm values should have been validated by arg parser: {}",
            value
        ),
    };

    let profile = args
        .value_of("profile")
        .ok_or(AppleCodesignError::CliBadArgument)?;
    let profile = CertificateProfile::from_str(profile)?;
    let team_id = args
        .value_of("team_id")
        .ok_or(AppleCodesignError::CliBadArgument)?;
    let person_name = args
        .value_of("person_name")
        .ok_or(AppleCodesignError::CliBadArgument)?;
    let country_name = args
        .value_of("country_name")
        .ok_or(AppleCodesignError::CliBadArgument)?;

    let validity_days = args.value_of("validity_days").unwrap();
    let validity_days =
        i64::from_str(validity_days).map_err(|_| AppleCodesignError::CliBadArgument)?;

    let validity_duration = chrono::Duration::days(validity_days);

    let (cert, _, raw) = create_self_signed_code_signing_certificate(
        algorithm,
        profile,
        team_id,
        person_name,
        country_name,
        validity_duration,
    )?;

    print!("{}", cert.encode_pem());
    print!(
        "{}",
        pem::encode(&pem::Pem {
            tag: "PRIVATE KEY".to_string(),
            contents: raw.as_ref().to_vec(),
        })
    );

    Ok(())
}

#[cfg(target_os = "macos")]
fn command_keychain_export_certificate_chain(args: &ArgMatches) -> Result<(), AppleCodesignError> {
    let user_id = args.value_of("user_id").unwrap();

    let domain = args
        .value_of("domain")
        .expect("clap should have added default value");

    let domain =
        KeychainDomain::try_from(domain).expect("clap should have validated domain values");

    let password = if let Some(path) = args.value_of("password_file") {
        let data = std::fs::read_to_string(path)?;

        Some(
            data.lines()
                .next()
                .expect("should get a single line")
                .to_string(),
        )
    } else if let Some(password) = args.value_of("password") {
        Some(password.to_string())
    } else {
        None
    };

    let certs = macos_keychain_find_certificate_chain(domain, password.as_deref(), user_id)?;

    for (i, cert) in certs.iter().enumerate() {
        if args.is_present("no_print_self") && i == 0 {
            continue;
        }

        print!("{}", cert.encode_pem());
    }

    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn command_keychain_export_certificate_chain(_args: &ArgMatches) -> Result<(), AppleCodesignError> {
    Err(AppleCodesignError::CliGeneralError(
        "macOS Keychain export only supported on macOS".to_string(),
    ))
}

fn command_parse_code_signing_requirement(args: &ArgMatches) -> Result<(), AppleCodesignError> {
    let path = args
        .value_of("input_path")
        .expect("clap should have validated argument");

    let data = std::fs::read(path)?;

    let requirements = CodeRequirements::parse_blob(&data)?.0;

    for requirement in requirements.iter() {
        match args
            .value_of("format")
            .expect("clap should have validated argument")
        {
            "csrl" => {
                println!("{}", requirement);
            }
            "expression-tree" => {
                println!("{:#?}", requirement);
            }
            format => panic!("unhandled format: {}", format),
        }
    }

    Ok(())
}

fn command_sign(args: &ArgMatches) -> Result<(), AppleCodesignError> {
    let log = get_logger();

    let mut settings = SigningSettings::default();

    let mut private_keys = vec![];
    let mut public_certificates = vec![];

    if let Some(pfx_path) = args.value_of("pfx_path") {
        let pfx_data = std::fs::read(pfx_path)?;

        let pfx_password = if let Some(password) = args.value_of("pfx_password") {
            password.to_string()
        } else if let Some(path) = args.value_of("pfx_password_file") {
            std::fs::read_to_string(path)?
                .lines()
                .next()
                .expect("should get a single line")
                .to_string()
        } else {
            error!(
                &log,
                "--pfx-password or --pfx-password-file must be specified"
            );
            return Err(AppleCodesignError::CliBadArgument);
        };

        let (cert, key) = parse_pfx_data(&pfx_data, &pfx_password)?;

        private_keys.push(key);
        public_certificates.push(cert);
    }

    if let Some(values) = args.values_of("pem_source") {
        for pem_source in values {
            warn!(&log, "reading PEM data from {}", pem_source);
            let pem_data = std::fs::read(pem_source)?;

            for pem in pem::parse_many(&pem_data) {
                match pem.tag.as_str() {
                    "CERTIFICATE" => {
                        public_certificates.push(CapturedX509Certificate::from_der(pem.contents)?);
                    }
                    "PRIVATE KEY" => {
                        private_keys.push(InMemorySigningKeyPair::from_pkcs8_der(&pem.contents)?)
                    }
                    tag => warn!(&log, "(unhandled PEM tag {}; ignoring)", tag),
                }
            }
        }
    }

    if private_keys.len() > 1 {
        error!(&log, "at most 1 PRIVATE KEY can be present; aborting");
        return Err(AppleCodesignError::CliBadArgument);
    }

    let private = if private_keys.is_empty() {
        None
    } else {
        Some(&private_keys[0])
    };

    if let Some(signing_key) = &private {
        if public_certificates.is_empty() {
            error!(
                &log,
                "a PRIVATE KEY requires a corresponding CERTIFICATE to pair with it"
            );
            return Err(AppleCodesignError::CliBadArgument);
        }

        let cert = public_certificates.remove(0);

        warn!(&log, "registering signing key");
        settings.set_signing_key(signing_key, cert);
        if let Some(certs) = settings.chain_apple_certificates() {
            for cert in certs {
                warn!(
                    &log,
                    "automatically registered Apple CA certificate: {}",
                    cert.subject_common_name()
                        .unwrap_or_else(|| "default".into())
                );
            }
        }

        if let Some(timestamp_url) = args.value_of("timestamp_url") {
            if timestamp_url != "none" {
                warn!(&log, "using time-stamp protocol server {}", timestamp_url);
                settings.set_time_stamp_url(timestamp_url)?;
                error!(&log, "use of time-stamp tokens is known to break Apple signature verification; see https://github.com/indygreg/PyOxidizer/issues/399");
            }
        }
    }

    for cert in public_certificates {
        warn!(&log, "registering extra X.509 certificate");
        settings.chain_certificate(cert);
    }

    if let Some(team_name) = args.value_of("team_name") {
        settings.set_team_id(team_name);
    }

    if let Some(value) = args.value_of("digest") {
        let digest_type = DigestType::try_from(value)?;
        settings.set_digest_type(digest_type);
    }

    if let Some(values) = args.values_of("binary_identifier") {
        for value in values {
            let (scope, identifier) = parse_scoped_value(value)?;
            settings.set_binary_identifier(scope, identifier);
        }
    }

    if let Some(values) = args.values_of("code_requirements_path") {
        for value in values {
            let (scope, path) = parse_scoped_value(value)?;

            let code_requirements_data = std::fs::read(path)?;
            let reqs = CodeRequirements::parse_blob(&code_requirements_data)?.0;
            for expr in reqs.iter() {
                warn!(
                    &log,
                    "setting designated code requirements for {}: {}", scope, expr
                );
                settings.set_designated_requirement_expression(scope.clone(), expr)?;
            }
        }
    }

    if let Some(values) = args.values_of("code_resources") {
        for value in values {
            let (scope, path) = parse_scoped_value(value)?;

            warn!(
                &log,
                "setting code resources data for {} from path {}", scope, path
            );
            let code_resources_data = std::fs::read(path)?;
            settings.set_code_resources_data(scope, code_resources_data);
        }
    }

    if let Some(values) = args.values_of("code_signature_flags_set") {
        for value in values {
            let (scope, value) = parse_scoped_value(value)?;

            let flags = CodeSignatureFlags::from_str(value)?;
            settings.set_code_signature_flags(scope, flags);
        }
    }

    if let Some(values) = args.values_of("entitlements_xml_path") {
        for value in values {
            let (scope, path) = parse_scoped_value(value)?;

            warn!(
                &log,
                "setting entitlments XML for {} from path {}", scope, path
            );
            let entitlements_data = std::fs::read_to_string(path)?;
            settings.set_entitlements_xml(scope, entitlements_data);
        }
    }

    if let Some(values) = args.values_of("executable_segment_flags_set") {
        for value in values {
            let (scope, value) = parse_scoped_value(value)?;

            let flags = ExecutableSegmentFlags::from_str(value)?;
            settings.set_executable_segment_flags(scope, flags);
        }
    }

    if let Some(values) = args.values_of("info_plist_path") {
        for value in values {
            let (scope, value) = parse_scoped_value(value)?;

            let content = std::fs::read(value)?;
            settings.set_info_plist_data(scope, content);
        }
    }

    let input_path = PathBuf::from(
        args.value_of("input_path")
            .expect("input_path presence should have been validated by clap"),
    );
    let output_path = args
        .value_of("output_path")
        .expect("output_path presence should have been validated by clap");

    if input_path.is_file() {
        warn!(&log, "signing {} as a Mach-O binary", input_path.display());
        let macho_data = std::fs::read(input_path)?;

        warn!(&log, "parsing Mach-O");
        let signer = MachOSigner::new(&macho_data)?;

        warn!(&log, "writing {}", output_path);
        let mut fh = std::fs::File::create(output_path)?;
        signer.write_signed_binary(&settings, &mut fh)?;
    } else {
        warn!(&log, "signing {} as a bundle", input_path.display());

        let signer = BundleSigner::new_from_path(&input_path)?;

        signer.write_signed_bundle(&log, &output_path, &settings)?;
    }

    Ok(())
}

fn command_verify(args: &ArgMatches) -> Result<(), AppleCodesignError> {
    let path = args
        .value_of("path")
        .ok_or(AppleCodesignError::CliBadArgument)?;

    let data = std::fs::read(path)?;

    let problems = verify::verify_macho_data(&data);

    for problem in &problems {
        println!("{}", problem);
    }

    if problems.is_empty() {
        eprintln!("no problems detected!");
        eprintln!("(we do not verify everything so please do not assume that the signature meets Apple standards)");
        Ok(())
    } else {
        Err(AppleCodesignError::VerificationProblems)
    }
}

fn main_impl() -> Result<(), AppleCodesignError> {
    let matches = App::new("Oxidized Apple Codesigning")
        .setting(AppSettings::ArgRequiredElseHelp)
        .version("0.1")
        .author("Gregory Szorc <gregory.szorc@gmail.com>")
        .about("Do things related to code signing of Apple binaries")
        .subcommand(
            SubCommand::with_name("compute-code-hashes")
                .about("Compute code hashes for a binary")
                .arg(
                    Arg::with_name("path")
                        .required(true)
                        .help("path to Mach-O binary to examine"),
                )
                .arg(
                    Arg::with_name("hash")
                        .long("hash")
                        .takes_value(true)
                        .possible_values(SUPPORTED_HASHES)
                        .default_value("sha256")
                        .help("Hashing algorithm to use"),
                )
                .arg(
                    Arg::with_name("page_size")
                        .long("page-size")
                        .takes_value(true)
                        .help("Chunk size to digest over"),
                )
                .arg(
                    Arg::with_name("universal_index")
                        .long("universal-index")
                        .takes_value(true)
                        .default_value("0")
                        .help("Index of Mach-O binary to operate on within a universal/fat binary"),
                ),
        )
        .subcommand(
            SubCommand::with_name("extract")
                .about("Extracts code signature data from a Mach-O binary")
                .long_about(EXTRACT_ABOUT)
                .arg(
                    Arg::with_name("path")
                        .required(true)
                        .help("Path to Mach-O binary to examine"),
                )
                .arg(
                    Arg::with_name("data")
                        .long("data")
                        .takes_value(true)
                        .possible_values(&[
                            "blobs",
                            "cms-pem",
                            "cms-raw",
                            "cms",
                            "code-directory-raw",
                            "code-directory-serialized-raw",
                            "code-directory-serialized",
                            "code-directory",
                            "linkedit-segment-raw",
                            "macho-load-commands",
                            "requirements-raw",
                            "requirements-rust",
                            "requirements-serialized-raw",
                            "requirements-serialized",
                            "requirements",
                            "segment-info",
                            "signature-raw",
                            "superblob",
                        ])
                        .default_value("segment-info")
                        .help("Which data to extract and how to format it"),
                )
                .arg(
                    Arg::with_name("universal_index")
                        .long("universal-index")
                        .takes_value(true)
                        .default_value("0")
                        .help("Index of Mach-O binary to operate on within a universal/fat binary"),
                ),
        )
        .subcommand(
            SubCommand::with_name("generate-self-signed-certificate")
                .about("Generate a self-signed certificate for code signing")
                .long_about(GENERATE_SELF_SIGNED_CERTIFICATE_ABOUT)
                .arg(
                    Arg::with_name("algorithm")
                        .long("algorithm")
                        .takes_value(true)
                        .possible_values(&["ecdsa", "ed25519"])
                        .default_value("ecdsa")
                        .help("Which key type to use"),
                )
                .arg(Arg::with_name("profile")
                    .long("profile")
                    .takes_value(true)
                    .possible_values(CertificateProfile::str_names())
                    .default_value("apple-development"))
                .arg(Arg::with_name("team_id")
                    .long("team-id")
                    .takes_value(true)
                    .default_value("unset")
                    .help("Team ID (this is a short string attached to your Apple Developer account)")
                )
                .arg(Arg::with_name("person_name")
                    .long("person-name")
                    .takes_value(true)
                    .required(true)
                    .help("The name of the person this certificate is for")
                )
                .arg(
                    Arg::with_name("country_name")
                        .long("country-name")
                        .takes_value(true)
                        .default_value("XX")
                        .help("Country Name (C) value for certificate identifier"),
                )
                .arg(
                    Arg::with_name("validity_days")
                        .long("validity-days")
                        .takes_value(true)
                        .default_value("365")
                        .help("How many days the certificate should be valid for"),
                ),
        ).
        subcommand(SubCommand::with_name("keychain-export-certificate-chain")
            .about("Export Apple CA certificates from the macOS Keychain")
            .arg(
                Arg::with_name("domain")
                    .long("--domain")
                    .possible_values(&["user", "system", "common", "dynamic"])
                    .default_value("user")
                    .help("Keychain domain to operate on")
            )
            .arg(
                Arg::with_name("password")
                    .long("--password")
                    .takes_value(true)
                    .help("Password to unlock the Keychain")
            )
            .arg(
                Arg::with_name("password_file")
                    .long("--password-file")
                    .takes_value(true)
                    .conflicts_with("password")
                    .help("File containing password to use to unlock the Keychain")
            )
           .arg(
                Arg::with_name("no_print_self")
                    .long("--no-print-self")
                    .help("Print only the issuing certificate chain, not the subject certificate")
           )
           .arg(
               Arg::with_name("user_id")
                    .long("--user-id")
                    .takes_value(true)
                    .required(true)
                    .help("User ID value of code signing certificate to find and whose CA chain to export")
           ),
        )
        .subcommand(SubCommand::with_name("parse-code-signing-requirement")
            .about("Parse binary Code Signing Requirement data into a human readable string")
            .long_about(PARSE_CODE_SIGNING_REQUIREMENT_ABOUT)
            .arg(
                Arg::with_name("format")
                    .long("--format")
                    .required(true)
                    .possible_values(&["csrl", "expression-tree"])
                    .default_value("csrl")
                    .help("Output format")
            )
            .arg(
                Arg::with_name("input_path")
                    .required(true)
                    .help("Path to file to parse")
            )
        )
        .subcommand(
            SubCommand::with_name("sign")
                .about("Sign a Mach-O binary or bundle")
                .long_about(SIGN_ABOUT)
                .arg(
                    Arg::with_name("binary_identifier")
                        .long("binary-identifier")
                        .takes_value(true)
                        .multiple(true)
                        .number_of_values(1)
                        .help("Identifier string for binary. The value normally used by CFBundleIdentifier")
                )
                .arg(
                    Arg::with_name("code_requirements_path")
                        .long("code-requirements-path")
                        .takes_value(true)
                        .multiple(true)
                        .number_of_values(1)
                        .help("Path to a file containing binary code requirements data to be used as designated requirements")
                )
                .arg(
                    Arg::with_name("code_resources")
                        .long("code-resources-path")
                        .takes_value(true)
                        .multiple(true)
                        .number_of_values(1)
                        .help("Path to an XML plist file containing code resources"),
                )
                .arg(
                    Arg::with_name("code_signature_flags_set")
                        .long("code-signature-flags")
                        .takes_value(true)
                        .help("Code signature flags to set")
                )
                .arg(
                    Arg::with_name("digest")
                        .long("digest")
                        .possible_values(SUPPORTED_HASHES)
                        .takes_value(true)
                        .default_value("sha256")
                        .help("Digest algorithm to use")
                )
                .arg(
                    Arg::with_name("entitlements_xml_path")
                        .long("entitlements-xml-path")
                        .short("e")
                        .takes_value(true)
                        .multiple(true)
                        .number_of_values(1)
                        .help("Path to a plist file containing entitlements"),
                )
                .arg(
                    Arg::with_name("executable_segment_flags_set")
                        .long("executable-segment-flags")
                        .takes_value(true)
                        .help("Executable segment flags to set")
                )
                .arg(
                    Arg::with_name("info_plist_path")
                        .long("info-plist-path")
                        .takes_value(true)
                        .help("Path to an Info.plist file whose digest to include in Mach-O signature")
                )
                .arg(
                    Arg::with_name("pem_source")
                        .long("pem-source")
                        .takes_value(true)
                        .multiple(true)
                        .help("Path to file containing PEM encoded certificate/key data"),
                )
                .arg(
                    Arg::with_name("pfx_path")
                        .long("--pfx-file")
                        .takes_value(true)
                        .help("Path to a PFX file containing a certificate key pair")
                )
                .arg(
                    Arg::with_name("pfx_password")
                        .long("--pfx-password")
                        .takes_value(true)
                        .help("The password to use to open the --pfx-file file")
                )
                .arg(
                    Arg::with_name("pfx_password_file")
                        .long("--pfx-password-file")
                        .conflicts_with("pfx_password")
                        .takes_value(true)
                        .help("Path to file containing password for opening --pfx-file file")
                )
                .arg(
                    Arg::with_name(
                        "team_name")
                        .long("team-name")
                        .takes_value(true)
                        .help("Team name/identifier to include in code signature"
                    )
                )
                .arg(
                    Arg::with_name("timestamp_url")
                        .long("timestamp-url")
                        .takes_value(true)
                        // TODO enable once https://github.com/indygreg/PyOxidizer/issues/399 is fixed.
                        //.default_value(APPLE_TIMESTAMP_URL)
                        .help(
                            "URL of timestamp server to use to obtain a token of the CMS signature",
                        ),
                )
                .arg(
                    Arg::with_name("input_path")
                        .required(true)
                        .help("Path to Mach-O binary to sign"),
                )
                .arg(
                    Arg::with_name("output_path")
                        .required(true)
                        .help("Path to signed Mach-O binary to write"),
                ),
        )
        .subcommand(
            SubCommand::with_name("verify")
                .about("Verifies code signature data")
                .arg(
                    Arg::with_name("path")
                        .required(true)
                        .help("Path of Mach-O binary to examine"),
                ),
        )
        .get_matches();

    match matches.subcommand() {
        ("compute-code-hashes", Some(args)) => command_compute_code_hashes(args),
        ("extract", Some(args)) => command_extract(args),
        ("generate-self-signed-certificate", Some(args)) => {
            command_generate_self_signed_certificate(args)
        }
        ("keychain-export-certificate-chain", Some(args)) => {
            command_keychain_export_certificate_chain(args)
        }
        ("parse-code-signing-requirement", Some(args)) => {
            command_parse_code_signing_requirement(args)
        }
        ("sign", Some(args)) => command_sign(args),
        ("verify", Some(args)) => command_verify(args),
        _ => Err(AppleCodesignError::CliUnknownCommand),
    }
}

fn main() {
    let exit_code = match main_impl() {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("Error: {:?}", err);
            1
        }
    };

    std::process::exit(exit_code)
}
