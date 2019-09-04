// src/io/mod.rs -- input/output interfaces for Tectonic.
// Copyright 2016-2018 the Tectonic Project
// Licensed under the MIT License.

//! Tectonic’s pluggable I/O backend.

use flate2::read::GzDecoder;
use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::str::FromStr;

use crate::ctry;
use crate::digest::{self, Digest, DigestData, FastDigestData};
use crate::errors::{Error, ErrorKind, Result};
use crate::status::StatusBackend;

pub mod cached_itarbundle;
pub mod filesystem;
pub mod format_cache;
pub mod memory;
pub mod setup;
pub mod stack;
pub mod stdstreams;
pub mod zipbundle;

pub trait InputFeatures: Read {
    fn get_size(&mut self) -> Result<usize>;
    fn try_seek(&mut self, pos: SeekFrom) -> Result<u64>;
}

/// What kind of source an input file ultimately came from. We keep track of
/// this in order to be able to emit Makefile-style dependencies for input
/// files. Right now, we only provide enough options to achieve this goal; we
/// could add more.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputOrigin {
    /// This file lives on the filesystem and might change under us. (That is
    /// it is not a cached bundle file.)
    Filesystem,

    /// This file was never used as an input.
    NotInput,

    /// This file is none of the above.
    Other,
}

/// Input handles are basically Read objects with a few extras. We don't
/// require the standard io::Seek because we need to provide a dummy
/// implementation for GZip streams, which we wouldn't be allowed to do
/// because both the trait and the target struct are outside of our crate.
///
/// An important role for the InputHandle struct is computing a cryptographic
/// digest of the input file. The driver uses this information in order to
/// figure out if the TeX engine needs rerunning. TeX makes our life more
/// difficult, though, since it has somewhat funky file access patterns. LaTeX
/// file opens work by opening a file and immediately closing it, which tests
/// whether the file exists, and then by opening it again for real. Under the
/// hood, XeTeX reads a couple of bytes from each file upon open to sniff its
/// encoding. So we can't just stream data from `read()` calls into the SHA2
/// computer, since we end up seeking and reading redundant data.
///
/// The current system maintains some internal state that, so far, helps us Do
/// The Right Thing given all this. If there's a seek on the file, we give up
/// on our digest computation. But if there's a seek back to the file
/// beginning, we are open to the possibility of restarting the computation.
/// But if nothing is ever read from the file, we once again give up on the
/// computation. The `ExecutionState` code then has further pieces that track
/// access to nonexistent files, which we treat as being equivalent to an
/// existing empty file for these purposes.
pub struct InputHandle {
    name: OsString,
    inner: Box<dyn InputFeatures>,
    /// Indicates that the file cannot be written to (provided by a read-only IoProvider) and
    /// therefore it is useless to compute the digest.
    read_only: bool,
    digest: digest::FastDigestComputer,
    origin: InputOrigin,
    ever_read: bool,
    did_unhandled_seek: bool,
    ungetc_char: Option<u8>,
}

impl InputHandle {
    pub fn new<T: 'static + InputFeatures>(
        name: &OsStr,
        inner: T,
        origin: InputOrigin,
    ) -> InputHandle {
        InputHandle {
            name: name.to_os_string(),
            inner: Box::new(inner),
            read_only: false,
            digest: Default::default(),
            origin,
            ever_read: false,
            did_unhandled_seek: false,
            ungetc_char: None,
        }
    }

    pub fn new_read_only<T: 'static + InputFeatures>(
        name: &OsStr,
        inner: T,
        origin: InputOrigin,
    ) -> InputHandle {
        InputHandle {
            name: name.to_os_string(),
            inner: Box::new(inner),
            read_only: true,
            digest: Default::default(),
            origin,
            ever_read: false,
            did_unhandled_seek: false,
            ungetc_char: None,
        }
    }

    pub fn name(&self) -> &OsStr {
        self.name.as_os_str()
    }

    pub fn origin(&self) -> InputOrigin {
        self.origin
    }

    /// Consumes the object and returns the underlying readable handle that
    /// it references.
    pub fn into_inner(self) -> Box<dyn InputFeatures> {
        self.inner
    }

    /// Consumes the object and returns the SHA256 sum of the content that was
    /// read. No digest is returned if there was ever a seek on the input
    /// stream, since in that case the results will not be reliable. We also
    /// return None if the stream was never read, which is another common
    /// TeX access pattern: files are opened, immediately closed, and then
    /// opened again. Finally, no digest is returned if the file is marked read-only.
    pub fn into_name_digest(self) -> (OsString, Option<FastDigestData>) {
        if self.did_unhandled_seek || !self.ever_read || self.read_only {
            (self.name, None)
        } else {
            (self.name, Some(FastDigestData::from(self.digest)))
        }
    }

    /// Various piece of TeX want to use the libc `ungetc()` function a lot.
    /// It's kind of gross, but happens often enough that we provide special
    /// support for it. Here's `getc()` emulation that can return a previously
    /// `ungetc()`-ed character.
    pub fn getc(&mut self) -> Result<u8> {
        if let Some(c) = self.ungetc_char {
            self.ungetc_char = None;
            return Ok(c);
        }

        let mut byte = [0u8; 1];

        if self.read(&mut byte[..1])? == 0 {
            // EOF
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "EOF in getc").into());
        }

        Ok(byte[0])
    }

    /// Here's the `ungetc()` emulation.
    pub fn ungetc(&mut self, byte: u8) -> Result<()> {
        if self.ungetc_char.is_some() {
            return Err(ErrorKind::Msg(
                "internal problem: cannot ungetc() more than once in a row".into(),
            )
            .into());
        }

        self.ungetc_char = Some(byte);
        Ok(())
    }
}

impl Read for InputHandle {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if !buf.is_empty() {
            if let Some(c) = self.ungetc_char {
                // This does sometimes happen, so we need to deal with it. It's not that bad really.
                buf[0] = c;
                self.ungetc_char = None;
                return Ok(self.read(&mut buf[1..])? + 1);
            }
        }

        self.ever_read = true;
        let n = self.inner.read(buf)?;
        if !self.read_only {
            self.digest.input(&buf[..n]);
        }
        Ok(n)
    }
}

impl InputFeatures for InputHandle {
    fn get_size(&mut self) -> Result<usize> {
        self.inner.get_size()
    }

    fn try_seek(&mut self, pos: SeekFrom) -> Result<u64> {
        match pos {
            SeekFrom::Start(0) => {
                // As described above, there is a common pattern in TeX file
                // accesses: read a few bytes to sniff, then go back to the
                // beginning. We should tidy up the I/O to just buffer instead
                // of seeking, but in the meantime, we can handle this.
                self.digest = Default::default();
                self.ever_read = false;
                self.ungetc_char = None;
            }
            SeekFrom::Current(0) => {
                // Noop. This must *not* clear the ungetc buffer for our
                // current PDF startxref/xref parsing code to work.
            }
            _ => {
                self.did_unhandled_seek = true;
                self.ungetc_char = None;
            }
        }

        let mut offset = self.inner.try_seek(pos)?;

        // If there was an ungetc, the effective position in the stream is one
        // byte before that of the underlying handle. Some of the code does
        // noop seeks to get the current offset for various file parsing
        // needs, so it's important that we return the right value. It should
        // never happen that the underlying stream thinks that the offset is
        // zero after we've ungetc'ed -- famous last words?

        if self.ungetc_char.is_some() {
            offset -= 1;
        }

        Ok(offset)
    }
}

pub struct OutputHandle {
    name: OsString,
    inner: Box<dyn Write>,
    digest: digest::FastDigestComputer,
}

impl OutputHandle {
    pub fn new<T: 'static + Write>(name: &OsStr, inner: T) -> OutputHandle {
        OutputHandle {
            name: name.to_os_string(),
            inner: Box::new(inner),
            digest: Default::default(),
        }
    }

    pub fn name(&self) -> &OsStr {
        self.name.as_os_str()
    }

    /// Consumes the object and returns the underlying writable handle that
    /// it references.
    pub fn into_inner(self) -> Box<dyn Write> {
        self.inner
    }

    /// Consumes the object and returns the SHA256 sum of the content that was
    /// written.
    pub fn into_name_digest(self) -> (OsString, FastDigestData) {
        (self.name, FastDigestData::from(self.digest))
    }
}

impl Write for OutputHandle {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.digest.input(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

// An Io provider is a source of handles. One wrinkle is that it's good to be
// able to distinguish between unavailability of a given name and error
// accessing it. We take file paths as OsStrs, although since we parse input
// files as Unicode it may not be possible to actually express zany
// non-Unicode Unix paths inside the engine.

#[derive(Debug)]
pub enum OpenResult<T> {
    Ok(T),
    NotAvailable,
    Err(Error),
}

impl<T> OpenResult<T> {
    pub fn unwrap(self) -> T {
        match self {
            OpenResult::Ok(t) => t,
            _ => panic!("expected an open file"),
        }
    }

    /// Returns true if this result is of the NotAvailable variant.
    pub fn is_not_available(&self) -> bool {
        if let OpenResult::NotAvailable = *self {
            true
        } else {
            false
        }
    }

    /// Convert this object into a plain Result, erroring if the item was not available.
    pub fn must_exist(self) -> Result<T> {
        match self {
            OpenResult::Ok(t) => Ok(t),
            OpenResult::Err(e) => Err(e),
            OpenResult::NotAvailable => {
                Err(io::Error::new(io::ErrorKind::NotFound, "not found").into())
            }
        }
    }
}

/// A hack to allow casting of Bundles to IoProviders.
///
/// The code that sets up the I/O stack is handed a reference to a Bundle
/// trait object. For the actual I/O, it needs to convert this to an
/// IoProvider trait object. [According to
/// StackExchange](https://stackoverflow.com/a/28664881/3760486), the
/// following pattern is the least-bad way to achieve the necessary upcasting.
pub trait AsIoProviderMut {
    /// Represent this value as an IoProvider trait object.
    fn as_ioprovider_mut(&mut self) -> &mut dyn IoProvider;
}

impl<T: IoProvider> AsIoProviderMut for T {
    fn as_ioprovider_mut(&mut self) -> &mut dyn IoProvider {
        self
    }
}

/// A trait for types that can read or write files needed by the TeX engine.
pub trait IoProvider: AsIoProviderMut {
    fn output_open_name(&mut self, _name: &OsStr) -> OpenResult<OutputHandle> {
        OpenResult::NotAvailable
    }

    fn output_open_stdout(&mut self) -> OpenResult<OutputHandle> {
        OpenResult::NotAvailable
    }

    fn input_open_name(
        &mut self,
        _name: &OsStr,
        _status: &mut dyn StatusBackend,
    ) -> OpenResult<InputHandle> {
        OpenResult::NotAvailable
    }

    /// Open the "primary" input file, which in the context of TeX is the main
    /// input that it's given. When the build is being done using the
    /// filesystem and the input is a file on the filesystem, this function
    /// isn't necesssarily that important, but those conditions don't always
    /// hold.
    fn input_open_primary(&mut self, _status: &mut dyn StatusBackend) -> OpenResult<InputHandle> {
        OpenResult::NotAvailable
    }

    /// Open a format file with the specified name. Format files have a
    /// specialized entry point because IOProviders may wish to handle them
    /// specially: namely, to munge the filename to one that includes the
    /// current version of the Tectonic engine, since the format contents
    /// depend sensitively on the engine internals.
    fn input_open_format(
        &mut self,
        name: &OsStr,
        status: &mut dyn StatusBackend,
    ) -> OpenResult<InputHandle> {
        self.input_open_name(name, status)
    }

    /// Save an a format dump in some way that this provider may be able to
    /// recover in the future. This awkward interface is needed for to write
    /// formats with their special munged file names.
    fn write_format(
        &mut self,
        _name: &str,
        _data: &[u8],
        _status: &mut dyn StatusBackend,
    ) -> Result<()> {
        Err(ErrorKind::Msg("this I/O layer cannot save format files".to_owned()).into())
    }
}

impl<P: IoProvider + ?Sized> IoProvider for Box<P> {
    fn output_open_name(&mut self, name: &OsStr) -> OpenResult<OutputHandle> {
        (**self).output_open_name(name)
    }

    fn output_open_stdout(&mut self) -> OpenResult<OutputHandle> {
        (**self).output_open_stdout()
    }

    fn input_open_name(
        &mut self,
        name: &OsStr,
        status: &mut dyn StatusBackend,
    ) -> OpenResult<InputHandle> {
        (**self).input_open_name(name, status)
    }

    fn input_open_primary(&mut self, status: &mut dyn StatusBackend) -> OpenResult<InputHandle> {
        (**self).input_open_primary(status)
    }

    fn input_open_format(
        &mut self,
        name: &OsStr,
        status: &mut dyn StatusBackend,
    ) -> OpenResult<InputHandle> {
        (**self).input_open_format(name, status)
    }

    fn write_format(
        &mut self,
        name: &str,
        data: &[u8],
        status: &mut dyn StatusBackend,
    ) -> Result<()> {
        (**self).write_format(name, data, status)
    }
}

/// A special IoProvider that can make TeX format files.
///
/// A “bundle” is expected to contain a large number of TeX support files —
/// for instance, a compilation of a TeXLive distribution. In terms of the
/// software architecture, though, what is special about a bundle is that one
/// can generate one or more TeX format files from its contents without
/// reference to any other I/O resources.
pub trait Bundle: IoProvider {
    /// Get a cryptographic digest summarizing this bundle’s contents.
    ///
    /// The digest summarizes the exact contents of every file in the bundle.
    /// It is computed from the sorted names and SHA256 digests of the
    /// component files [as implemented in the script
    /// builder/make-zipfile.py](https://github.com/tectonic-typesetting/tectonic-staging/blob/master/builder/make-zipfile.py#L138)
    /// in the `tectonic-staging` module.
    ///
    /// The default implementation gets the digest from a file name
    /// `SHA256SUM`, which is expected to contain the digest in hex-encoded
    /// format.
    fn get_digest(&mut self, status: &mut dyn StatusBackend) -> Result<DigestData> {
        let digest_text = match self.input_open_name(OsStr::new(digest::DIGEST_NAME), status) {
            OpenResult::Ok(h) => {
                let mut text = String::new();
                h.take(64).read_to_string(&mut text)?;
                text
            }

            OpenResult::NotAvailable => {
                // Broken or un-cacheable backend.
                return Err(ErrorKind::Msg(
                    "itar-format bundle does not provide needed SHA256SUM file".to_owned(),
                )
                .into());
            }

            OpenResult::Err(e) => {
                return Err(e);
            }
        };

        Ok(ctry!(DigestData::from_str(&digest_text); "corrupted SHA256 digest data"))
    }
}

impl<B: Bundle + ?Sized> Bundle for Box<B> {
    fn get_digest(&mut self, status: &mut dyn StatusBackend) -> Result<DigestData> {
        (**self).get_digest(status)
    }
}

// Some generically helpful InputFeatures impls

impl<R: Read> InputFeatures for GzDecoder<R> {
    fn get_size(&mut self) -> Result<usize> {
        Err(ErrorKind::NotSizeable.into())
    }

    fn try_seek(&mut self, _: SeekFrom) -> Result<u64> {
        Err(ErrorKind::NotSeekable.into())
    }
}

impl InputFeatures for Cursor<Vec<u8>> {
    fn get_size(&mut self) -> Result<usize> {
        Ok(self.get_ref().len())
    }

    fn try_seek(&mut self, pos: SeekFrom) -> Result<u64> {
        Ok(self.seek(pos)?)
    }
}

// Reexports

pub use self::filesystem::{FilesystemIo, FilesystemPrimaryInputIo};
pub use self::memory::MemoryIo;
pub use self::setup::{IoSetup, IoSetupBuilder};
pub use self::stack::IoStack;
pub use self::stdstreams::GenuineStdoutIo;

// Helpful.

pub fn try_open_file<P: AsRef<Path>>(path: P) -> OpenResult<File> {
    use std::io::ErrorKind::NotFound;

    match File::open(path) {
        Ok(f) => OpenResult::Ok(f),
        Err(e) => {
            if e.kind() == NotFound {
                OpenResult::NotAvailable
            } else {
                OpenResult::Err(e.into())
            }
        }
    }
}

/// Normalize a TeX path in a system independent™ way by stripping any `.`, `..`,
/// or extra separators '/' so that it is of the form
///
/// ```text
/// path/to/my/file.txt
/// ../../path/to/parent/dir/file.txt
/// /absolute/path/to/file.txt
/// ```
///
/// Does not strip whitespace.
///
/// Returns `None` if the path refers to a parent of the root.
fn try_normalize_tex_path(path: &str) -> Option<String> {
    use std::iter::repeat;
    if path.is_empty() {
        return Some("".into());
    }
    let mut r = Vec::new();
    let mut parent_level = 0;
    let mut has_root = false;

    // TODO: We need to handle a prefix on Windows (i.e. "C:").

    for (i, c) in path.split('/').enumerate() {
        match c {
            "" if i == 0 => {
                has_root = true;
                r.push("");
            }
            "" | "." => {}
            ".." => {
                match r.pop() {
                    // about to pop the root
                    Some("") => return None,
                    None => parent_level += 1,
                    _ => {}
                }
            }
            _ => r.push(c),
        }
    }

    let r = repeat("..")
        .take(parent_level)
        .chain(r.into_iter())
        // No `join` on `Iterator`.
        .collect::<Vec<_>>()
        .join("/");

    if r.is_empty() {
        if has_root {
            Some("/".into())
        } else {
            Some(".".into())
        }
    } else {
        Some(r)
    }
}

/// Normalize a TeX path if possible, otherwise return the original path.
///
/// _TeX path_ is a path that obeys simplified semantics: Unix-like syntax (`/` for separators, etc.),
/// must be Unicode-able, no symlinks allowed such that `..` can be stripped lexically.
///
/// TODO: This function should operate on `&str` someday, but we need to transition the internals
/// away from `OsStr/OsString` before that can happen.
fn normalize_tex_path(path: &OsStr) -> Cow<OsStr> {
    if let Some(t) = path
        .to_str()
        .and_then(try_normalize_tex_path)
        .map(OsString::from)
    {
        Cow::Owned(t)
    } else {
        Cow::Borrowed(path)
    }
}

// Helper for testing. FIXME: I want this to be conditionally compiled with
// #[cfg(test)] but things break if I do that.

pub mod testing {
    use super::*;
    use std::ffi::{OsStr, OsString};
    use std::fs::File;
    use std::path::{Path, PathBuf};

    pub struct SingleInputFileIo {
        name: OsString,
        full_path: PathBuf,
    }

    impl SingleInputFileIo {
        pub fn new(path: &Path) -> SingleInputFileIo {
            let p = path.to_path_buf();

            SingleInputFileIo {
                name: p.file_name().unwrap().to_os_string(),
                full_path: p,
            }
        }
    }

    impl IoProvider for SingleInputFileIo {
        fn output_open_name(&mut self, _: &OsStr) -> OpenResult<OutputHandle> {
            OpenResult::NotAvailable
        }

        fn output_open_stdout(&mut self) -> OpenResult<OutputHandle> {
            OpenResult::NotAvailable
        }

        fn input_open_name(
            &mut self,
            name: &OsStr,
            _status: &mut dyn StatusBackend,
        ) -> OpenResult<InputHandle> {
            if name == self.name {
                OpenResult::Ok(InputHandle::new(
                    name,
                    File::open(&self.full_path).unwrap(),
                    InputOrigin::Filesystem,
                ))
            } else {
                OpenResult::NotAvailable
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_try_normalize_tex_path() {
        // edge cases
        assert_eq!(try_normalize_tex_path(""), Some("".into()));
        assert_eq!(try_normalize_tex_path("/"), Some("/".into()));
        assert_eq!(try_normalize_tex_path("//"), Some("/".into()));
        assert_eq!(try_normalize_tex_path("."), Some(".".into()));
        assert_eq!(try_normalize_tex_path("./"), Some(".".into()));
        assert_eq!(try_normalize_tex_path(".."), Some("..".into()));
        assert_eq!(try_normalize_tex_path("././/./"), Some(".".into()));
        assert_eq!(try_normalize_tex_path("/././/."), Some("/".into()));

        assert_eq!(
            try_normalize_tex_path("my/path/file.txt"),
            Some("my/path/file.txt".into())
        );
        // preserve spaces
        assert_eq!(
            try_normalize_tex_path("  my/pa  th/file .txt "),
            Some("  my/pa  th/file .txt ".into())
        );
        assert_eq!(
            try_normalize_tex_path("/my/path/file.txt"),
            Some("/my/path/file.txt".into())
        );
        assert_eq!(
            try_normalize_tex_path("./my///path/././file.txt"),
            Some("my/path/file.txt".into())
        );
        assert_eq!(
            try_normalize_tex_path("./../my/../../../file.txt"),
            Some("../../../file.txt".into())
        );
        assert_eq!(
            try_normalize_tex_path("././my//../path/../here/file.txt"),
            Some("here/file.txt".into())
        );
        assert_eq!(
            try_normalize_tex_path("./my/.././/path/../../here//file.txt"),
            Some("../here/file.txt".into())
        );

        assert_eq!(try_normalize_tex_path("/my/../../file.txt"), None);
        assert_eq!(
            try_normalize_tex_path("/my/./.././path//../../file.txt"),
            None
        );
    }
}
