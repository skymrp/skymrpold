// use std::fs::File;
// use std::io::{BufRead, Read};
// use std::path::Path;
// use byteorder::ByteOrder;
// use encoding_rs::GBK;
// use flate2::read::GzDecoder;

// use crate::errors::{Error,Result};

// pub struct GetMrpInfoOption {
//     pub gunzip: bool,
// }

// #[derive(Debug, Default, Clone)]
// pub struct MrpFile {
//     pub filename: String,
//     pub position: u32,
//     pub size: u32,
//     pub data: Vec<u8>,
//     pub data_gziped: Option<Vec<u8>>,
// }

// #[derive(Debug, Default, Clone)]
// pub struct MrpHeader {
//     pub magic:u32,
//     pub mrp_type: String,
//     pub version: i32,
//     pub author: String,
//     pub description: String,
//     pub show_name: String,
//     pub internal_name: String,
//     pub number: i32,
//     pub bytes: u32,
//     pub file_list_from: u32,
//     pub data_from: u32,
//     pub files: Vec<MrpFile>,
// }

// impl Default for GetMrpInfoOption {
//     fn default() -> Self {
//         Self { gunzip: false }
//     }
// }


// fn parse_mach_header<T: BufRead, O: ByteOrder>(magic: u32, buf: &mut T) -> Result<MrpHeader> {
//     Ok(MrpHeader { magic, mrp_type: (), version: (), author: (), description: (), show_name: (), internal_name: (), number: (), bytes: (), file_list_from: (), data_from: (), files: () })
// }




// fn string_from_buffer(buffer: &[u8], from: usize, to: usize) -> Result<String, Error> {
//     if from >= buffer.len() || to > buffer.len() || from > to {
//         return Err(Error::ReadInfoError("Buffer index out of bounds".to_string()));
//     }
    
//     let slice = &buffer[from..to];
    
//     // find first 0x00
//     let end_idx = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
//     let valid_slice = &slice[..end_idx];
    
//     let (cow, _, had_errors) = GBK.decode(valid_slice);
//     if had_errors {
//         // Fallback or just ignore errors as it replaces with replacement char
//         // In original JS, it tries truncating from the end if it fails.
//         // For simplicity, we just return the decoded string with replacements.
//     }
    
//     Ok(cow.into_owned().trim().to_string())
// }

// pub fn get_mrp_info(content: &[u8], option: Option<GetMrpInfoOption>) -> Result<MrpHeader, Error> {
//     let opt = option.unwrap_or_default();
    
//     if content.len() < 200 {
//         return Err(Error::ReadInfoError("File too small".to_string()));
//     }

//     let mrp_type = string_from_buffer(content, 0, 4)?;
//     if mrp_type != "MRPG" {
//         return Err(Error::NotMrpError);
//     }

//     let data_from = u32::from_le_bytes(content[4..8].try_into().unwrap());
//     let bytes = u32::from_le_bytes(content[8..12].try_into().unwrap());
//     let file_list_from = u32::from_le_bytes(content[12..16].try_into().unwrap());
    
//     let internal_name = string_from_buffer(content, 16, 28)?;
//     let show_name = string_from_buffer(content, 28, 52)?;
    
//     let mut number = i32::from_be_bytes(content[192..196].try_into().unwrap());
//     if number < 0 {
//         number = i32::from_le_bytes(content[68..72].try_into().unwrap());
//     }
    
//     let mut version = i32::from_be_bytes(content[196..200].try_into().unwrap());
//     if version < 0 {
//         version = i32::from_le_bytes(content[196..200].try_into().unwrap());
//     }
    
//     let author = string_from_buffer(content, 88, 128)?;
//     let description = string_from_buffer(content, 128, 192)?;

//     let mut files = Vec::new();
//     let mut read_from = file_list_from as usize;
    
//     while read_from < content.len() && read_from < (data_from as usize + 8) {
//         if read_from + 4 > content.len() {
//             break;
//         }
//         let name_len = u32::from_le_bytes(content[read_from..read_from+4].try_into().unwrap()) as usize;
//         read_from += 4;
        
//         if name_len == 0 || read_from + name_len > content.len() {
//             break;
//         }
        
//         let filename = string_from_buffer(content, read_from, read_from + name_len - 1)?;
//         read_from += name_len;
        
//         if read_from + 12 > content.len() {
//             break;
//         }
        
//         let file_pos = u32::from_le_bytes(content[read_from..read_from+4].try_into().unwrap());
//         read_from += 4;
        
//         let file_len = u32::from_le_bytes(content[read_from..read_from+4].try_into().unwrap());
//         read_from += 8; // skip file_len and next 4 empty bytes
        
//         let file_pos_usize = file_pos as usize;
//         let file_len_usize = file_len as usize;
        
//         if file_pos_usize + file_len_usize > content.len() {
//             return Err(Error::ReadInfoError(format!("File {} data out of bounds", filename)));
//         }
        
//         let data = content[file_pos_usize .. file_pos_usize + file_len_usize].to_vec();
        
//         let data_gziped = if opt.gunzip {
//             if data.len() >= 2 && data[0] == 0x1f && data[1] == 0x8b {
//                 let mut decoder = GzDecoder::new(&data[..]);
//                 let mut decompressed = Vec::new();
//                 decoder.read_to_end(&mut decompressed).map_err(|e| Error::GunzipError(e.to_string()))?;
//                 Some(decompressed)
//             } else {
//                 Some(data.clone())
//             }
//         } else {
//             None
//         };
        
//         files.push(MrpFile {
//             filename,
//             position: file_pos,
//             size: file_len,
//             data,
//             data_gziped,
//         });
//     }

//     Ok(MrpHeader {
//         0,
//         mrp_type,
//         version,
//         author,
//         description,
//         show_name,
//         internal_name,
//         number,
//         bytes,
//         file_list_from,
//         data_from,
//         files,
//     })
// }

// pub fn get_mrp_info_from_file<P: AsRef<Path>>(path: P, option: Option<GetMrpInfoOption>) -> Result<MrpHeader, Error> {
//     let mut file = File::open(path)?;
//     let mut buffer = Vec::new();
//     file.read_to_end(&mut buffer)?;
//     get_mrp_info(&buffer, option)
// }

// #[cfg(test)]
// pub mod tests {

// }
