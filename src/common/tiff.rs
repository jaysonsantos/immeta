use std::io::{Read, Seek, SeekFrom};
use std::cell::{RefCell, Cell};
use std::marker::PhantomData;

use byteorder;

use types::Result;
use utils::{ByteOrder, ByteOrderReadExt};

/// A TIFF document reader.
///
/// This structure wraps a `Read` and `Seek` implementation and allows one to read a TIFF
/// document from it.
pub struct TiffReader<R: Read + Seek> {
    source: R
}

impl<R: Read + Seek> TiffReader<R> {
    /// Wraps the provider `Read + Seek` implementation and returns a new TIFF reader.
    pub fn new(source: R) -> TiffReader<R> {
        TiffReader {
            source: source
        }
    }

    /// Returns an iterator over IFDs in the TIFF document.
    ///
    /// This method first checks that the underlying data stream is indeed a valid TIFF document,
    /// and only then returns the iterator.
    ///
    /// Note that the returned value does not implement `IntoIterator`, but an immutable
    /// reference to it does. Therefore, it should be used like this:
    ///
    /// ```no_run
    /// # use std::io::Cursor;
    /// # use immeta::common::tiff::TiffReader;
    /// # let r = TiffReader::new(Cursor::new(Vec::<u8>::new()));
    /// for ifd in &r.ifds().unwrap() {
    ///     // ...
    /// }
    /// ```
    pub fn ifds(mut self) -> Result<LazyIfds<R>> {
        let mut bom = [0u8; 2];
        try_if_eof!(std, self.source.read_exact(&mut bom), "while reading byte order mark");

        let byte_order = match &bom {
            b"II" => ByteOrder::Little,
            b"MM" => ByteOrder::Big,
            _ => return Err(invalid_format!("invalid TIFF BOM: {:?}", bom))
        };

        let magic = try_if_eof!(
            self.source.read_u16(byte_order),
            "when reading TIFF magic number"
        );
        if magic != 42 {
            return Err(invalid_format!("invalid TIFF magic number: {}", magic));
        }

        Ok(LazyIfds {
            source: RefCell::new(self.source),
            byte_order: byte_order,
            next_ifd_offset: Cell::new(4),
        })
    }
}

/// An intermediate structure, a reference to which can be converted to an iterator
/// of IFDs.
pub struct LazyIfds<R: Read + Seek> {
    source: RefCell<R>,
    byte_order: ByteOrder,
    next_ifd_offset: Cell<u64>,
}

impl<'a, R: Read + Seek> IntoIterator for &'a LazyIfds<R> {
    type Item = Result<Ifd<'a, R>>;
    type IntoIter = Ifds<'a, R>;

    fn into_iter(self) -> Ifds<'a, R> {
        Ifds(self)
    }
}

/// An iterator of IFDs in a TIFF document.
pub struct Ifds<'a, R: Read + Seek + 'a>(&'a LazyIfds<R>);

impl<'a, R: Read + Seek + 'a> Iterator for Ifds<'a, R> {
    type Item = Result<Ifd<'a, R>>;

    fn next(&mut self) -> Option<Result<Ifd<'a, R>>> {
        match self.read_ifd() {
            Ok(value) => value.map(Ok),
            Err(e) => Some(Err(e)),
        }
    }
}

impl<'a, R: Read + Seek> Ifds<'a, R> {
    fn read_ifd(&mut self) -> Result<Option<Ifd<'a, R>>> {
        let next_ifd_offset = self.0.next_ifd_offset.get();

        // next ifd offset is only zero in the last entry of a TIFF document
        if next_ifd_offset == 0 {
            return Ok(None);
        }

        // seek to the beginning of the next IFD
        try_if_eof!(std,
            self.0.source.borrow_mut().seek(SeekFrom::Start(next_ifd_offset as u64)),
            "when seeking to the beginning of the next IFD"
        );
        let current_ifd_offset = next_ifd_offset;

        // read the length of this IFD
        let current_ifd_size = try_if_eof!(
            self.0.source.borrow_mut().read_u16(self.0.byte_order), "when reading number of entries in an IFD"
        );
        // it is an error for an IFD to be empty
        if current_ifd_size == 0 {
            return Err(invalid_format!("number of entries in an IFD is zero"));
        }

        // compute the offset of the next IFD offset and seek to it
        let next_ifd_offset_offset = current_ifd_offset + 2 + current_ifd_size as u64 * 12;
        try_if_eof!(std,
            self.0.source.borrow_mut().seek(SeekFrom::Start(next_ifd_offset_offset as u64)),
            "when seeking to the next IFD offset"
        );

        // read and update the next IFD offset for further calls to `next()`
        self.0.next_ifd_offset.set(try_if_eof!(
            self.0.source.borrow_mut().read_u16(self.0.byte_order), "when reading the next IFD offset"
        ) as u64);

        Ok(Some(Ifd {
            ifds: self.0,
            ifd_offset: current_ifd_offset,
            current_entry: 0,
            total_entries: current_ifd_size,
        }))
    }
}

/// Represents a single IFD.
///
/// A TIFF IFD consists of entries, so this structure is an iterator yielding IFD entries.
pub struct Ifd<'a, R: Read + Seek + 'a> {
    ifds: &'a LazyIfds<R>,
    ifd_offset: u64,
    current_entry: u16,
    total_entries: u16,
}

impl<'a, R: Read + Seek + 'a> Iterator for Ifd<'a, R> {
    type Item = Result<Entry<'a, R>>;

    fn next(&mut self) -> Option<Result<Entry<'a, R>>> {
        if self.current_entry == self.total_entries {
            None
        } else {
            Some(self.read_entry())
        }
    }
}

impl<'a, R: Read + Seek + 'a> Ifd<'a, R> {
    fn read_entry(&mut self) -> Result<Entry<'a, R>> {
        let mut source = self.ifds.source.borrow_mut();

        // seek to the beginning of the next entry (ifd offset + 2 + next_entry * 12)
        try!(source.seek(SeekFrom::Start(self.ifd_offset + 2 + self.current_entry as u64 * 12)));

        // read the tag
        let tag = try_if_eof!(
            source.read_u16(self.ifds.byte_order), "when reading TIFF IFD entry tag"
        );

        // read the entry type
        let entry_type = try_if_eof!(
            source.read_u16(self.ifds.byte_order), "when reading TIFF IFD entry type"
        );

        // read the count
        let count = try_if_eof!(
            source.read_u32(self.ifds.byte_order), "when reading TIFF IFD entry data count"
        );

        // read the offset/value
        let offset = try_if_eof!(
            source.read_u32(self.ifds.byte_order), "when reading TIFF IFD entry data offset"
        );

        self.current_entry += 1;

        Ok(Entry {
            ifds: self.ifds,
            tag: tag,
            entry_type: entry_type.into(),
            count: count,
            offset: offset,
        })
    }
}

/// Designates TIFF IFD entry type, as defined by TIFF spec.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum EntryType {
    Byte,
    Ascii,
    Short,
    Long,
    Rational,
    SignedByte,
    Undefined,
    SignedShort,
    SignedLong,
    SignedRational,
    Float,
    Double,
    Unknown(u16),
}

impl From<u16> for EntryType {
    fn from(n: u16) -> EntryType {
        match n {
            1  => EntryType::Byte,
            2  => EntryType::Ascii,
            3  => EntryType::Short,
            4  => EntryType::Long,
            5  => EntryType::Rational,
            6  => EntryType::SignedByte,
            7  => EntryType::Undefined,
            8  => EntryType::SignedShort,
            9  => EntryType::SignedLong,
            10 => EntryType::SignedRational,
            11 => EntryType::Float,
            12 => EntryType::Double,
            n  => EntryType::Unknown(n),
        }
    }
}

impl EntryType {
    fn size(self) -> Option<u8> {
        match self {
            EntryType::Byte           => Some(1),
            EntryType::Ascii          => Some(1),
            EntryType::Short          => Some(2),
            EntryType::Long           => Some(4),
            EntryType::Rational       => Some(8),
            EntryType::SignedByte     => Some(1),
            EntryType::Undefined      => Some(1),
            EntryType::SignedShort    => Some(2),
            EntryType::SignedLong     => Some(4),
            EntryType::SignedRational => Some(4),
            EntryType::Float          => Some(4),
            EntryType::Double         => Some(8),
            EntryType::Unknown(_)     => None,
        }
    }
}

/// Represents a single TIFF IFD entry.
pub struct Entry<'a, R: Read + Seek + 'a> {
    ifds: &'a LazyIfds<R>,
    tag: u16,
    entry_type: EntryType,
    count: u32,
    offset: u32,
}

impl<'a, R: Read + Seek + 'a> Entry<'a, R> {
    /// Returns the tag of the entry.
    #[inline]
    pub fn tag(&self) -> u16 {
        self.tag
    }

    /// Returns entry type.
    #[inline]
    pub fn entry_type(&self) -> EntryType {
        self.entry_type
    }

    /// Returns the number of items this entry contains.
    #[inline]
    pub fn count(&self) -> u32 {
        self.count
    }

    /// Returns an iterator for elements of the specified representation type.
    ///
    /// This method returns `None` if the requested representation type does not correspond
    /// to the actual type of the entry. Also it returns `None` if the entry type is
    /// unknown.
    #[inline]
    pub fn values<T: EntryTypeRepr>(&self) -> Option<EntryValues<'a, T, R>> {
        // compare the requested repr type with the actual entry type
        if self.entry_type == T::entry_type() {
            // then try to get the size and ignore the data in the entry if it is unknown
            if let Some(entry_type_size) = T::entry_type().size() {
                // if the total entry data size is smaller than 4 bytes (u32 value length)
                // the the data is embedded into the offset u32
                if entry_type_size as u32 * self.count <= 4 {
                    Some(EntryValues::Embedded(EmbeddedValues {
                        current: 0,
                        count: self.count,
                        data: self.offset,
                        _entry_type_repr: PhantomData,
                    }))
                // othewise the data is stored at that offset
                } else {
                    Some(EntryValues::Referenced(ReferencedValues {
                        ifds: self.ifds,
                        current: 0,
                        count: self.count,
                        next_offset: self.offset,
                        _entry_type_repr: PhantomData,
                    }))
                }
            } else {
                None
            }
        } else {
            None
        }
    }

    /// Returns a vector containing all of the items of this entry, loaded with the specified
    /// representation type.
    ///
    /// This method returns `None` if the requested representation type does not correspond
    /// to the actual type of the entry. Also it returns `None` if the entry type is
    /// unknown.
    #[inline]
    pub fn all_values<T: EntryTypeRepr>(&self) -> Option<Result<Vec<T::Repr>>> {
        // compare the requested repr type with the actual entry type
        if self.entry_type == T::entry_type() {
            // then try to get the size and ignore the data in the entry if it is unknown
            if let Some(entry_type_size) = T::entry_type().size() {
                // if the total entry data size is smaller than 4 bytes (u32 value length)
                // the the data is embedded into the offset u32, and we just delegate to the
                // iterator
                if entry_type_size as u32 * self.count <= 4 {
                    Some(self.values::<T>().unwrap().collect())
                // othewise the data is stored at that offset, load it all at once
                } else {
                    match self.ifds.source.borrow_mut().seek(SeekFrom::Start(self.offset as u64))
                        .map_err(if_eof!(std, "when seeking to the beginning of IFD entry data"))
                    {
                        Ok(_) => {}
                        Err(e) => return Some(Err(e))
                    }

                    let mut result = Vec::new();
                    match T::read_many_from(&mut *self.ifds.source.borrow_mut(),
                                            self.ifds.byte_order, self.count, &mut result)
                        .map_err(if_eof!("when reading TIFF IFD entry values"))
                    {
                        Ok(_) => Some(Ok(result)),
                        Err(e) => Some(Err(e))
                    }
                }

            } else {
                None
            }

        } else {
            None
        }
    }
}

/// Designates a marker type which represent one of TIFF directory entry types.
pub trait EntryTypeRepr {
    /// The represented type, e.g. Rust primitive or a string.
    type Repr;

    /// Returns the entry type corresponding to this marker type.
    fn entry_type() -> EntryType;

    /// Attempts to read the represented value from the given stream with the given byte order.
    ///
    /// Returns the number of bytes read and the value itself.
    fn read_from<R: Read>(source: &mut R, byte_order: ByteOrder) -> byteorder::Result<(u32, Self::Repr)>;

    /// Attempts to read a number of the represented values from the given stream with the given
    /// byte order.
    ///
    /// `n` values will be are stored in `target`, or an error will be returned. `target` vector
    /// may be modified even if this method returns an error.
    fn read_many_from<R: Read>(source: &mut R, byte_order: ByteOrder, n: u32, target: &mut Vec<Self::Repr>) -> byteorder::Result<()>;

    /// Reads the `n`th represented value inside `source`.
    ///
    /// If the value can be read successfully (`n` < `count`, the represented type is smaller
    /// than or equal to u32, etc.), returns `Some(value)`, otherwise returns `None`.
    fn read_from_u32(source: u32, n: u32, count: u32) -> Option<Self::Repr>;
}

/// Contains representation types for all of defined TIFF entry types.
pub mod entry_types {
    use std::io::Read;
    use std::mem;
    use std::str;

    use byteorder;
    use arrayvec::ArrayVec;

    use super::{EntryType, EntryTypeRepr};
    use utils::{ByteOrder, ByteOrderReadExt};

    macro_rules! gen_entry_types {
        (
            $(
                $tpe:ident, $repr:ty,
                |$source:pat, $byte_order:pat| $read:expr,
                |$u32_source:pat, $n:pat, $count:pat| $u32_read:expr
            );+
        ) => {
            $(
                pub enum $tpe {}

                impl EntryTypeRepr for $tpe {
                    type Repr = $repr;

                    #[inline]
                    fn entry_type() -> EntryType {
                        EntryType::$tpe
                    }

                    fn read_from<R: Read>($source: &mut R, $byte_order: ByteOrder) -> byteorder::Result<(u32, $repr)> {
                        $read
                    }

                    fn read_many_from<R: Read>(source: &mut R, byte_order: ByteOrder,
                                               n: u32, target: &mut Vec<Self::Repr>) -> byteorder::Result<()> {
                        for _ in 0..n {
                            target.push(try!(Self::read_from(source, byte_order)).1);
                        }
                        Ok(())
                    }

                    fn read_from_u32($u32_source: u32, $n: u32, $count: u32) -> Option<$repr> {
                        $u32_read
                    }
                }
            )+
        }
    }

    // s = zzzzzzzz yyyyyyyy xxxxxxxx wwwwwwww
    // n =    3         2        1        0
    #[inline]
    fn nbyte(s: u32, n: u32) -> u8 {
        assert!(n <= 3);
        ((s >> 8 * (3 - n)) & 0xFF) as u8
    }

    gen_entry_types! {
        Byte, u8,
            |source, _| byteorder::ReadBytesExt::read_u8(source).map(|v| (1, v)),
            |source, n, count| if n >= count || n >= 4 { None } else { Some(nbyte(source, n)) };
        Ascii, String,
            |source, _| {
                let mut s = String::new();
                loop {
                    let b = try!(byteorder::ReadBytesExt::read_u8(source));
                    if b == 0 { break; }
                    s.push(b as char);
                }
                Ok((s.len() as u32 + 1, s))
            },
            |source, n, count| if n >= count || n >= 4 { None } else {
                // w x y z
                // +-----0   4
                // 0 +---0   4
                // +---0 0   3, 4
                // 0 +-0 0   3, 4
                // +-0 +-0   2, 4
                // +-0 0 0   2, 3, 4
                // 0 0 +-0   1, 2, 4
                // 0 0 0 0   1, 2, 3, 4
                let bs = [nbyte(source, 0), nbyte(source, 1), nbyte(source, 2), nbyte(source, 3)];
                fn find_substrings<A: Extend<(usize, usize)>>(s: &[u8], target: &mut A) {
                    let mut p = 0;
                    let mut i = 0;
                    while i < s.len() {
                        if s[i] == 0 {
                            target.extend(Some((p, i)));  // excluding zero byte
                            p = i+1;
                        }
                        i += 1;
                    }
                }
                let mut substrings = ArrayVec::<[_; 4]>::new();
                find_substrings(&bs[..count as usize], &mut substrings);
                substrings.get(n as usize)
                    .map(|&(s, e)| unsafe { str::from_utf8_unchecked(&bs[s..e]).to_owned() })
            };
        Short, u16,
            |source, byte_order| source.read_u16(byte_order).map(|v| (2, v)),
            |source, n, count| if n >= count || n >= 2 { None } else {
                Some(
                    ((nbyte(source, 2*n + 1) as u16) << 8) |
                    (nbyte(source, 2*n) as u16)
                )
            };
        Long, u32,
            |source, byte_order| source.read_u32(byte_order).map(|v| (4, v)),
            |source, n, _| if n != 1 { None } else {
                Some(
                    ((nbyte(source, 3) as u32) << 24) |
                    ((nbyte(source, 2) as u32) << 16) |
                    ((nbyte(source, 1) as u32) << 8) |
                    (nbyte(source, 0) as u32)
                )
            };
        Rational, (u32, u32),
            |source, byte_order| source.read_u32(byte_order)
                .and_then(|n| source.read_u32(byte_order).map(|d| (n, d)))
                .map(|v| (4 * 2, v)),
            |_, _, _| None;
        SignedByte, i8,
            |source, _| byteorder::ReadBytesExt::read_i8(source).map(|v| (1, v)),
            |source, n, count| if n >= count || n >= 4 { None } else { Some(nbyte(source, n) as i8) };
        Undefined, u8,
            |source, _| byteorder::ReadBytesExt::read_u8(source).map(|v| (1, v)),
            |source, n, count| if n >= count || n >= 4 { None } else { Some(nbyte(source, n)) };
        SignedShort, i16,
            |source, byte_order| source.read_i16(byte_order).map(|v| (2, v)),
            |source, n, count| if n >= count || n >= 2 { None } else {
                Some(
                    ((nbyte(source, 2*n + 1) as i16) << 8) |
                    (nbyte(source, 2*n) as i16)
                )
            };
        SignedLong, i32,
            |source, byte_order| source.read_i32(byte_order).map(|v| (4, v)),
            |source, n, _| if n >= 1 { None } else {
                Some(
                    ((nbyte(source, 3) as i32) << 24) |
                    ((nbyte(source, 2) as i32) << 16) |
                    ((nbyte(source, 1) as i32) << 8) |
                    (nbyte(source, 0) as i32)
                )
            };
        SignedRational, (i32, i32),
            |source, byte_order| source.read_i32(byte_order)
                .and_then(|n| source.read_i32(byte_order).map(|d| (n, d)))
                .map(|v| (4 * 2, v)),
            |_, _, _| None;
        Float, f32,
            |source, byte_order| source.read_f32(byte_order).map(|v| (4, v)),
            |source, n, _| if n >= 1 { None } else { Some(unsafe { mem::transmute(source) }) };
        Double, f64,
            |source, byte_order| source.read_f64(byte_order).map(|v| (8, v)),
            |_, _, _| None
    }
}

/// An iterator over values in an TIFF IFD entry.
pub enum EntryValues<'a, T: EntryTypeRepr, R: Read + Seek + 'a> {
    #[doc(hidden)]
    Embedded(EmbeddedValues<T>),
    #[doc(hidden)]
    Referenced(ReferencedValues<'a, T, R>),
}

impl<'a, T: EntryTypeRepr, R: Read + Seek + 'a> Iterator for EntryValues<'a, T, R> {
    type Item = Result<T::Repr>;

    fn next(&mut self) -> Option<Result<T::Repr>> {
        match self.read_value() {
            Ok(result) => result.map(Ok),
            Err(e) => Some(Err(e))
        }
    }
}

impl<'a, T: EntryTypeRepr, R: Read + Seek + 'a> EntryValues<'a, T, R> {
    fn read_value(&mut self) -> Result<Option<T::Repr>> {
        match *self {
            EntryValues::Embedded(ref mut v) => Ok(v.read_value()),
            EntryValues::Referenced(ref mut v) => v.read_value(),
        }
    }
}

#[doc(hidden)]
pub struct EmbeddedValues<T: EntryTypeRepr> {
    current: u32,
    count: u32,
    data: u32,
    _entry_type_repr: PhantomData<T>,
}

impl<T: EntryTypeRepr> EmbeddedValues<T> {
    fn read_value(&mut self) -> Option<T::Repr> {
        if self.current >= self.count {
            None
        } else {
            let result = T::read_from_u32(self.data, self.current, self.count);
            self.current += 1;
            result
        }
    }
}

#[doc(hidden)]
pub struct ReferencedValues<'a, T: EntryTypeRepr, R: Read + Seek + 'a> {
    ifds: &'a LazyIfds<R>,
    current: u32,
    count: u32,
    next_offset: u32,
    _entry_type_repr: PhantomData<T>,
}

impl<'a, T: EntryTypeRepr, R: Read + Seek + 'a> ReferencedValues<'a, T, R> {
    fn read_value(&mut self) -> Result<Option<T::Repr>> {
        if self.current >= self.count {
            return Ok(None);
        }

        try!(self.ifds.source.borrow_mut().seek(SeekFrom::Start(self.next_offset as u64)));

        let (bytes_read, value) = try_if_eof!(
            T::read_from(&mut *self.ifds.source.borrow_mut(), self.ifds.byte_order),
            "when reading TIFF entry value"
        );
        self.next_offset += bytes_read;
        self.current += 1;

        Ok(Some(value))
    }
}