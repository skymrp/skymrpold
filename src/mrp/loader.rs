use std::fs::File;
use std::io::Read;
use std::path::Path;

use encoding_rs::GBK;
use flate2::read::GzDecoder;

use crate::errors::{Error, Result};

const MRP_MAGIC: &[u8; 4] = b"MRPG";
const LEGACY_NEW_FORMAT_CUTOFF: u32 = 232;
const MIN_HEADER_SIZE: usize = 200;
const MAX_FILENAME_SIZE: usize = 256;
const MAX_FILE_SIZE: u32 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MrpFormat {
    Old,
    New,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct GetMrpInfoOption {
    pub gunzip: bool,
}

#[derive(Debug, Clone)]
pub struct MrpFile {
    pub filename: String,
    pub position: u32,
    pub size: u32,
    pub data: Vec<u8>,
    pub data_gziped: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct MrpHeader {
    pub magic: u32,
    pub mrp_type: String,
    pub format: MrpFormat,
    pub version: i32,
    pub author: String,
    pub description: String,
    pub show_name: String,
    pub internal_name: String,
    pub number: i32,
    pub bytes: u32,
    pub file_list_from: u32,
    pub data_from: u32,
    pub files: Vec<MrpFile>,
}

#[derive(Debug, Clone)]
pub struct MrpPackage {
    header: MrpHeader,
}

impl MrpPackage {
    pub fn parse(content: &[u8], option: Option<GetMrpInfoOption>) -> Result<Self> {
        Ok(Self {
            header: get_mrp_info(content, option)?,
        })
    }

    pub fn from_file<P: AsRef<Path>>(path: P, option: Option<GetMrpInfoOption>) -> Result<Self> {
        Ok(Self {
            header: get_mrp_info_from_file(path, option)?,
        })
    }

    pub fn header(&self) -> &MrpHeader {
        &self.header
    }

    pub fn files(&self) -> &[MrpFile] {
        &self.header.files
    }

    pub fn file(&self, filename: &str) -> Option<&MrpFile> {
        self.header
            .files
            .iter()
            .find(|file| file.filename == filename)
    }

    pub fn read_file(&self, filename: &str) -> Option<&[u8]> {
        self.file(filename).map(|file| file.data.as_slice())
    }

    pub fn read_file_unzipped(&self, filename: &str) -> Result<Option<Vec<u8>>> {
        let Some(file) = self.file(filename) else {
            return Ok(None);
        };
        decode_possible_gzip(&file.data).map(Some)
    }
}

pub fn get_mrp_info(content: &[u8], option: Option<GetMrpInfoOption>) -> Result<MrpHeader> {
    let opt = option.unwrap_or_default();

    if content.len() < MIN_HEADER_SIZE {
        return Err(Error::ReadInfoError("file too small".to_string()));
    }

    if content.get(0..4) != Some(MRP_MAGIC.as_slice()) {
        return Err(Error::NotMrpError);
    }

    let magic = read_u32_le(content, 0)?;
    let data_from = read_u32_le(content, 4)?;
    let bytes = read_u32_le(content, 8)?;
    let file_list_from = read_u32_le(content, 12)?;
    let format = if data_from > LEGACY_NEW_FORMAT_CUTOFF {
        MrpFormat::New
    } else {
        MrpFormat::Old
    };

    let internal_name = string_from_buffer(content, 16, 28)?;
    let show_name = string_from_buffer(content, 28, 52)?;
    let number = read_metadata_i32(content, 192, 68)?;
    let version = read_metadata_i32(content, 196, 196)?;
    let author = string_from_buffer(content, 88, 128)?;
    let description = string_from_buffer(content, 128, 192)?;

    let files = match format {
        MrpFormat::New => parse_new_format_files(content, data_from, bytes, file_list_from, opt)?,
        MrpFormat::Old => parse_old_format_files(content, data_from, opt)?,
    };

    Ok(MrpHeader {
        magic,
        mrp_type: "MRPG".to_string(),
        format,
        version,
        author,
        description,
        show_name,
        internal_name,
        number,
        bytes,
        file_list_from,
        data_from,
        files,
    })
}

pub fn get_mrp_info_from_file<P: AsRef<Path>>(
    path: P,
    option: Option<GetMrpInfoOption>,
) -> Result<MrpHeader> {
    let mut file = File::open(path)?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)?;
    get_mrp_info(&buffer, option)
}

fn parse_new_format_files(
    content: &[u8],
    data_from: u32,
    bytes: u32,
    file_list_from: u32,
    opt: GetMrpInfoOption,
) -> Result<Vec<MrpFile>> {
    let index_start = usize_from_u32(file_list_from)?;
    let index_end = usize_from_u32(checked_add_u32(data_from, 8)?)?;
    let index = slice_range(content, index_start, index_end)?;

    let mut files = Vec::new();
    let mut pos = 0usize;
    while pos < index.len() {
        let name_len = read_u32_le(index, pos)? as usize;
        pos = checked_add_usize(pos, 4)?;
        if name_len == 0 || name_len >= MAX_FILENAME_SIZE {
            return Err(Error::ReadInfoError(format!(
                "invalid filename length {name_len} in MRP index"
            )));
        }

        let name_end = checked_add_usize(pos, name_len)?;
        let filename = string_from_buffer(index, pos, name_end)?;
        pos = name_end;

        if checked_add_usize(pos, 12)? > index.len() {
            return Err(Error::ReadInfoError(format!(
                "truncated index entry for {filename}"
            )));
        }
        let file_pos = read_u32_le(index, pos)?;
        let file_len = read_u32_le(index, pos + 4)?;
        pos = checked_add_usize(pos, 12)?;

        validate_file_bounds(content, bytes, file_pos, file_len, &filename)?;
        files.push(build_file(content, filename, file_pos, file_len, opt)?);
    }

    Ok(files)
}

fn parse_old_format_files(
    content: &[u8],
    data_from: u32,
    opt: GetMrpInfoOption,
) -> Result<Vec<MrpFile>> {
    let mut files = Vec::new();
    let mut pos = usize_from_u32(checked_add_u32(data_from, 8)?)?;

    while pos < content.len() {
        if content.len() - pos < 4 {
            break;
        }

        let name_len = read_u32_le(content, pos)? as usize;
        pos = checked_add_usize(pos, 4)?;
        if name_len == 0 || name_len >= MAX_FILENAME_SIZE {
            break;
        }

        let name_end = match pos.checked_add(name_len) {
            Some(end) if end <= content.len() => end,
            _ => break,
        };
        let filename = string_from_buffer(content, pos, name_end)?;
        pos = name_end;

        if content.len() - pos < 4 {
            return Err(Error::ReadInfoError(format!(
                "truncated data length for {filename}"
            )));
        }
        let file_len = read_u32_le(content, pos)?;
        pos = checked_add_usize(pos, 4)?;
        if file_len == 0 || file_len > MAX_FILE_SIZE {
            return Err(Error::ReadInfoError(format!(
                "invalid data length {file_len} for {filename}"
            )));
        }

        let file_pos = u32_from_usize(pos)?;
        validate_content_range(content, pos, file_len as usize, &filename)?;
        files.push(build_file(content, filename, file_pos, file_len, opt)?);
        pos = checked_add_usize(pos, file_len as usize)?;
    }

    Ok(files)
}

fn build_file(
    content: &[u8],
    filename: String,
    file_pos: u32,
    file_len: u32,
    opt: GetMrpInfoOption,
) -> Result<MrpFile> {
    let file_pos_usize = usize_from_u32(file_pos)?;
    let file_len_usize = usize_from_u32(file_len)?;
    let data = slice_len(content, file_pos_usize, file_len_usize)?.to_vec();
    let data_gziped = if opt.gunzip {
        Some(decode_possible_gzip(&data)?)
    } else {
        None
    };

    Ok(MrpFile {
        filename,
        position: file_pos,
        size: file_len,
        data,
        data_gziped,
    })
}

fn decode_possible_gzip(data: &[u8]) -> Result<Vec<u8>> {
    if !is_gzip(data) {
        return Ok(data.to_vec());
    }

    let mut decoder = GzDecoder::new(data);
    let mut decompressed = Vec::new();
    decoder
        .read_to_end(&mut decompressed)
        .map_err(|err| Error::GunzipError(err.to_string()))?;
    Ok(decompressed)
}

fn is_gzip(data: &[u8]) -> bool {
    matches!(data, [0x1f, 0x8b, ..])
}

fn string_from_buffer(buffer: &[u8], from: usize, to: usize) -> Result<String> {
    if from > to || to > buffer.len() {
        return Err(Error::OutOfRange(from, 0..buffer.len()));
    }

    let slice = &buffer[from..to];
    let end_idx = slice
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(slice.len());
    let valid_slice = &slice[..end_idx];
    let (decoded, _, _) = GBK.decode(valid_slice);
    Ok(decoded.into_owned().trim().to_string())
}

fn read_metadata_i32(content: &[u8], primary_offset: usize, fallback_offset: usize) -> Result<i32> {
    let primary = read_i32_be(content, primary_offset)?;
    if primary < 0 {
        read_i32_le(content, fallback_offset)
    } else {
        Ok(primary)
    }
}

fn validate_file_bounds(
    content: &[u8],
    bytes: u32,
    file_pos: u32,
    file_len: u32,
    filename: &str,
) -> Result<()> {
    if file_len == 0 || file_len > MAX_FILE_SIZE {
        return Err(Error::ReadInfoError(format!(
            "invalid data length {file_len} for {filename}"
        )));
    }

    let Some(end) = file_pos.checked_add(file_len) else {
        return Err(Error::ReadInfoError(format!(
            "file {filename} data overflows MRP bounds"
        )));
    };
    if end > bytes {
        return Err(Error::ReadInfoError(format!(
            "file {filename} data out of MRP bounds"
        )));
    }

    let file_pos_usize = usize_from_u32(file_pos)?;
    let file_len_usize = usize_from_u32(file_len)?;
    validate_content_range(content, file_pos_usize, file_len_usize, filename)
}

fn validate_content_range(
    content: &[u8],
    file_pos: usize,
    file_len: usize,
    filename: &str,
) -> Result<()> {
    if file_pos
        .checked_add(file_len)
        .map_or(true, |end| end > content.len())
    {
        return Err(Error::ReadInfoError(format!(
            "file {filename} data out of buffer bounds"
        )));
    }
    Ok(())
}

fn slice_len(buffer: &[u8], from: usize, len: usize) -> Result<&[u8]> {
    let to = checked_add_usize(from, len)?;
    slice_range(buffer, from, to)
}

fn slice_range(buffer: &[u8], from: usize, to: usize) -> Result<&[u8]> {
    if from > to || to > buffer.len() {
        return Err(Error::OutOfRange(from, 0..buffer.len()));
    }
    Ok(&buffer[from..to])
}

fn read_u32_le(buffer: &[u8], offset: usize) -> Result<u32> {
    let bytes: [u8; 4] = slice_len(buffer, offset, 4)?
        .try_into()
        .map_err(|_| Error::BufferOverflow(offset))?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_i32_le(buffer: &[u8], offset: usize) -> Result<i32> {
    let bytes: [u8; 4] = slice_len(buffer, offset, 4)?
        .try_into()
        .map_err(|_| Error::BufferOverflow(offset))?;
    Ok(i32::from_le_bytes(bytes))
}

fn read_i32_be(buffer: &[u8], offset: usize) -> Result<i32> {
    let bytes: [u8; 4] = slice_len(buffer, offset, 4)?
        .try_into()
        .map_err(|_| Error::BufferOverflow(offset))?;
    Ok(i32::from_be_bytes(bytes))
}

fn checked_add_u32(lhs: u32, rhs: u32) -> Result<u32> {
    lhs.checked_add(rhs).ok_or(Error::NumberOverflow)
}

fn checked_add_usize(lhs: usize, rhs: usize) -> Result<usize> {
    lhs.checked_add(rhs).ok_or(Error::NumberOverflow)
}

fn usize_from_u32(value: u32) -> Result<usize> {
    value.try_into().map_err(|_| Error::NumberOverflow)
}

fn u32_from_usize(value: usize) -> Result<u32> {
    value.try_into().map_err(|_| Error::NumberOverflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER_METADATA_SIZE: usize = 240;

    fn base_header() -> Vec<u8> {
        let mut content = vec![0; HEADER_METADATA_SIZE];
        content[0..4].copy_from_slice(MRP_MAGIC);
        content[16..20].copy_from_slice(b"demo");
        content[28..32].copy_from_slice(b"Demo");
        content[88..94].copy_from_slice(b"author");
        content[128..132].copy_from_slice(b"desc");
        content[192..196].copy_from_slice(&1_i32.to_be_bytes());
        content[196..200].copy_from_slice(&2_i32.to_be_bytes());
        content
    }

    #[test]
    fn parses_new_format_index() {
        let mut content = base_header();
        let index_offset = HEADER_METADATA_SIZE;
        let file_offset = 320usize;
        let file_data = b"hello";
        content.resize(index_offset, 0);

        content.extend_from_slice(&(9u32.to_le_bytes()));
        content.extend_from_slice(b"start.mr\0");
        content.extend_from_slice(&(file_offset as u32).to_le_bytes());
        content.extend_from_slice(&(file_data.len() as u32).to_le_bytes());
        content.extend_from_slice(&0u32.to_le_bytes());

        content.resize(file_offset, 0);
        content.extend_from_slice(file_data);

        let index_len = 4 + 9 + 12;
        content[4..8].copy_from_slice(&((index_offset + index_len - 8) as u32).to_le_bytes());
        let content_len = content.len() as u32;
        content[8..12].copy_from_slice(&content_len.to_le_bytes());
        content[12..16].copy_from_slice(&(index_offset as u32).to_le_bytes());

        let header = get_mrp_info(&content, None).unwrap();
        assert_eq!(header.format, MrpFormat::New);
        assert_eq!(header.files.len(), 1);
        assert_eq!(header.files[0].filename, "start.mr");
        assert_eq!(header.files[0].data, file_data);
    }

    #[test]
    fn parses_old_format_inline_data() {
        let mut content = base_header();
        let data_from = 232u32;
        let entries_start = (data_from + 8) as usize;
        content.resize(entries_start, 0);
        content.extend_from_slice(&(9u32.to_le_bytes()));
        content.extend_from_slice(b"start.mr\0");
        content.extend_from_slice(&(5u32.to_le_bytes()));
        content.extend_from_slice(b"hello");

        content[4..8].copy_from_slice(&data_from.to_le_bytes());
        let content_len = content.len() as u32;
        content[8..12].copy_from_slice(&content_len.to_le_bytes());
        content[12..16].copy_from_slice(&(entries_start as u32).to_le_bytes());

        let header = get_mrp_info(&content, None).unwrap();
        assert_eq!(header.format, MrpFormat::Old);
        assert_eq!(header.files.len(), 1);
        assert_eq!(header.files[0].filename, "start.mr");
        assert_eq!(header.files[0].data, b"hello");
    }
}
