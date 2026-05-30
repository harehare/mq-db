use std::collections::HashSet;

use crate::{
    block::{Block, BlockType, Properties, PropertyValue, Span},
    document::ZoneMaps,
    error::MqdbError,
};

fn invalid_data(message: impl Into<String>) -> MqdbError {
    MqdbError::Storage(message.into())
}

fn as_u8(value: usize, field: &str) -> u8 {
    u8::try_from(value).unwrap_or_else(|_| panic!("{field} exceeds u8 range"))
}

fn as_u16(value: usize, field: &str) -> u16 {
    u16::try_from(value).unwrap_or_else(|_| panic!("{field} exceeds u16 range"))
}

fn as_u32(value: usize, field: &str) -> u32 {
    u32::try_from(value).unwrap_or_else(|_| panic!("{field} exceeds u32 range"))
}

fn usize_from_u32(value: u32, field: &str) -> Result<usize, MqdbError> {
    usize::try_from(value).map_err(|_| invalid_data(format!("{field} exceeds usize range")))
}

struct Decoder<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Decoder<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn read_exact(&mut self, len: usize) -> Result<&'a [u8], MqdbError> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or_else(|| invalid_data("byte offset overflow"))?;
        if end > self.data.len() {
            return Err(invalid_data("unexpected end of input"));
        }
        let bytes = &self.data[self.pos..end];
        self.pos = end;
        Ok(bytes)
    }

    fn read_u8(&mut self) -> Result<u8, MqdbError> {
        Ok(self.read_exact(1)?[0])
    }

    fn read_u16(&mut self) -> Result<u16, MqdbError> {
        let bytes: [u8; 2] = self
            .read_exact(2)?
            .try_into()
            .map_err(|_| invalid_data("failed to read u16"))?;
        Ok(u16::from_le_bytes(bytes))
    }

    fn read_u32(&mut self) -> Result<u32, MqdbError> {
        let bytes: [u8; 4] = self
            .read_exact(4)?
            .try_into()
            .map_err(|_| invalid_data("failed to read u32"))?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn read_i64(&mut self) -> Result<i64, MqdbError> {
        let bytes: [u8; 8] = self
            .read_exact(8)?
            .try_into()
            .map_err(|_| invalid_data("failed to read i64"))?;
        Ok(i64::from_le_bytes(bytes))
    }

    fn read_f64(&mut self) -> Result<f64, MqdbError> {
        let bytes: [u8; 8] = self
            .read_exact(8)?
            .try_into()
            .map_err(|_| invalid_data("failed to read f64"))?;
        Ok(f64::from_le_bytes(bytes))
    }

    fn read_string_u16(&mut self) -> Result<String, MqdbError> {
        let len = usize::from(self.read_u16()?);
        let bytes = self.read_exact(len)?;
        String::from_utf8(bytes.to_vec())
            .map_err(|e| invalid_data(format!("invalid UTF-8 string: {e}")))
    }

    fn read_string_u32(&mut self) -> Result<String, MqdbError> {
        let len = usize_from_u32(self.read_u32()?, "string length")?;
        let bytes = self.read_exact(len)?;
        String::from_utf8(bytes.to_vec())
            .map_err(|e| invalid_data(format!("invalid UTF-8 string: {e}")))
    }
}

fn encode_block_type(block_type: &BlockType) -> u8 {
    match block_type {
        BlockType::Heading => 0,
        BlockType::Paragraph => 1,
        BlockType::Code => 2,
        BlockType::List => 3,
        BlockType::TableCell => 4,
        BlockType::TableRow => 5,
        BlockType::TableAlign => 6,
        BlockType::Blockquote => 7,
        BlockType::HorizontalRule => 8,
        BlockType::Html => 9,
        BlockType::Yaml => 10,
        BlockType::Toml => 11,
        BlockType::Math => 12,
        BlockType::Definition => 13,
        BlockType::Footnote => 14,
    }
}

fn decode_block_type(value: u8) -> Result<BlockType, MqdbError> {
    match value {
        0 => Ok(BlockType::Heading),
        1 => Ok(BlockType::Paragraph),
        2 => Ok(BlockType::Code),
        3 => Ok(BlockType::List),
        4 => Ok(BlockType::TableCell),
        5 => Ok(BlockType::TableRow),
        6 => Ok(BlockType::TableAlign),
        7 => Ok(BlockType::Blockquote),
        8 => Ok(BlockType::HorizontalRule),
        9 => Ok(BlockType::Html),
        10 => Ok(BlockType::Yaml),
        11 => Ok(BlockType::Toml),
        12 => Ok(BlockType::Math),
        13 => Ok(BlockType::Definition),
        14 => Ok(BlockType::Footnote),
        _ => Err(invalid_data(format!("unknown block type tag: {value}"))),
    }
}

fn encode_property_value(value: &PropertyValue, out: &mut Vec<u8>) {
    match value {
        PropertyValue::Null => out.push(0x00),
        PropertyValue::String(s) => {
            out.push(0x01);
            out.extend_from_slice(&as_u32(s.len(), "string length").to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        PropertyValue::Int(i) => {
            out.push(0x02);
            out.extend_from_slice(&i.to_le_bytes());
        }
        PropertyValue::Float(f) => {
            out.push(0x03);
            out.extend_from_slice(&f.to_le_bytes());
        }
        PropertyValue::Bool(b) => {
            out.push(0x04);
            out.push(u8::from(*b));
        }
        PropertyValue::Array(values) => {
            out.push(0x05);
            out.extend_from_slice(&as_u16(values.len(), "array length").to_le_bytes());
            for value in values {
                encode_property_value(value, out);
            }
        }
    }
}

fn decode_property_value(decoder: &mut Decoder<'_>) -> Result<PropertyValue, MqdbError> {
    match decoder.read_u8()? {
        0x00 => Ok(PropertyValue::Null),
        0x01 => Ok(PropertyValue::String(decoder.read_string_u32()?)),
        0x02 => Ok(PropertyValue::Int(decoder.read_i64()?)),
        0x03 => Ok(PropertyValue::Float(decoder.read_f64()?)),
        0x04 => match decoder.read_u8()? {
            0 => Ok(PropertyValue::Bool(false)),
            1 => Ok(PropertyValue::Bool(true)),
            value => Err(invalid_data(format!("invalid bool tag: {value}"))),
        },
        0x05 => {
            let count = usize::from(decoder.read_u16()?);
            let mut values = Vec::with_capacity(count);
            for _ in 0..count {
                values.push(decode_property_value(decoder)?);
            }
            Ok(PropertyValue::Array(values))
        }
        value => Err(invalid_data(format!("unknown property value tag: {value}"))),
    }
}

fn encode_len_prefixed_u16(value: &str, out: &mut Vec<u8>) {
    out.extend_from_slice(&as_u16(value.len(), "string length").to_le_bytes());
    out.extend_from_slice(value.as_bytes());
}

fn sorted_strings(set: &HashSet<String>) -> Vec<&str> {
    let mut values: Vec<&str> = set.iter().map(String::as_str).collect();
    values.sort_unstable();
    values
}

pub fn encode_block(block: &Block) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&block.id.to_le_bytes());
    out.extend_from_slice(&block.document_id.to_le_bytes());
    out.push(encode_block_type(&block.block_type));
    out.extend_from_slice(&block.pre.to_le_bytes());
    out.extend_from_slice(&block.post.to_le_bytes());

    match &block.span {
        Some(span) => {
            out.push(1);
            out.extend_from_slice(&as_u32(span.start_line, "span.start_line").to_le_bytes());
            out.extend_from_slice(&as_u32(span.start_col, "span.start_col").to_le_bytes());
            out.extend_from_slice(&as_u32(span.end_line, "span.end_line").to_le_bytes());
            out.extend_from_slice(&as_u32(span.end_col, "span.end_col").to_le_bytes());
        }
        None => out.push(0),
    }

    out.extend_from_slice(&as_u32(block.content.len(), "content length").to_le_bytes());
    out.extend_from_slice(block.content.as_bytes());

    let mut properties: Vec<(&String, &PropertyValue)> = block.properties.iter().collect();
    properties.sort_unstable_by_key(|(left, _)| *left);

    out.extend_from_slice(&as_u16(properties.len(), "property count").to_le_bytes());
    for (key, value) in properties {
        out.push(as_u8(key.len(), "property key length"));
        out.extend_from_slice(key.as_bytes());
        encode_property_value(value, &mut out);
    }

    out
}

pub fn decode_block(data: &[u8]) -> Result<(Block, usize), MqdbError> {
    let mut decoder = Decoder::new(data);

    let id = decoder.read_u32()?;
    let document_id = decoder.read_u32()?;
    let block_type = decode_block_type(decoder.read_u8()?)?;
    let pre = decoder.read_u32()?;
    let post = decoder.read_u32()?;
    let span = match decoder.read_u8()? {
        0 => None,
        1 => Some(Span {
            start_line: usize_from_u32(decoder.read_u32()?, "span.start_line")?,
            start_col: usize_from_u32(decoder.read_u32()?, "span.start_col")?,
            end_line: usize_from_u32(decoder.read_u32()?, "span.end_line")?,
            end_col: usize_from_u32(decoder.read_u32()?, "span.end_col")?,
        }),
        value => return Err(invalid_data(format!("invalid span presence tag: {value}"))),
    };

    let content = decoder.read_string_u32()?;

    let prop_count = usize::from(decoder.read_u16()?);
    let mut properties = Properties::new();
    for _ in 0..prop_count {
        let key_len = usize::from(decoder.read_u8()?);
        let key = String::from_utf8(decoder.read_exact(key_len)?.to_vec())
            .map_err(|e| invalid_data(format!("invalid property key UTF-8: {e}")))?;
        let value = decode_property_value(&mut decoder)?;
        properties.set(key, value);
    }

    Ok((
        Block {
            id,
            document_id,
            block_type,
            content,
            span,
            pre,
            post,
            properties,
        },
        decoder.pos,
    ))
}

pub fn encode_zone_map(zm: &ZoneMaps) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(zm.max_heading_depth);

    let heading_slugs = sorted_strings(&zm.heading_slugs);
    out.extend_from_slice(&as_u16(heading_slugs.len(), "heading slug count").to_le_bytes());
    for value in heading_slugs {
        encode_len_prefixed_u16(value, &mut out);
    }

    let heading_contents = sorted_strings(&zm.heading_contents);
    out.extend_from_slice(&as_u16(heading_contents.len(), "heading content count").to_le_bytes());
    for value in heading_contents {
        encode_len_prefixed_u16(value, &mut out);
    }

    let code_languages = sorted_strings(&zm.code_languages);
    out.extend_from_slice(&as_u16(code_languages.len(), "code language count").to_le_bytes());
    for value in code_languages {
        encode_len_prefixed_u16(value, &mut out);
    }

    let frontmatter_keys = sorted_strings(&zm.frontmatter_keys);
    out.extend_from_slice(&as_u16(frontmatter_keys.len(), "frontmatter key count").to_le_bytes());
    for value in frontmatter_keys {
        encode_len_prefixed_u16(value, &mut out);
    }

    match &zm.title {
        Some(title) => {
            out.push(1);
            encode_len_prefixed_u16(title, &mut out);
        }
        None => out.push(0),
    }

    out.extend_from_slice(&as_u16(zm.tags.len(), "tag count").to_le_bytes());
    for tag in &zm.tags {
        encode_len_prefixed_u16(tag, &mut out);
    }

    out
}

pub fn decode_zone_map(data: &[u8]) -> Result<ZoneMaps, MqdbError> {
    let mut decoder = Decoder::new(data);

    let max_heading_depth = decoder.read_u8()?;

    let heading_slug_count = usize::from(decoder.read_u16()?);
    let mut heading_slugs = HashSet::with_capacity(heading_slug_count);
    for _ in 0..heading_slug_count {
        heading_slugs.insert(decoder.read_string_u16()?);
    }

    let heading_content_count = usize::from(decoder.read_u16()?);
    let mut heading_contents = HashSet::with_capacity(heading_content_count);
    for _ in 0..heading_content_count {
        heading_contents.insert(decoder.read_string_u16()?);
    }

    let code_language_count = usize::from(decoder.read_u16()?);
    let mut code_languages = HashSet::with_capacity(code_language_count);
    for _ in 0..code_language_count {
        code_languages.insert(decoder.read_string_u16()?);
    }

    let frontmatter_key_count = usize::from(decoder.read_u16()?);
    let mut frontmatter_keys = HashSet::with_capacity(frontmatter_key_count);
    for _ in 0..frontmatter_key_count {
        frontmatter_keys.insert(decoder.read_string_u16()?);
    }

    let title = match decoder.read_u8()? {
        0 => None,
        1 => Some(decoder.read_string_u16()?),
        value => return Err(invalid_data(format!("invalid title presence tag: {value}"))),
    };

    let tag_count = usize::from(decoder.read_u16()?);
    let mut tags = Vec::with_capacity(tag_count);
    for _ in 0..tag_count {
        tags.push(decoder.read_string_u16()?);
    }

    Ok(ZoneMaps {
        max_heading_depth,
        heading_slugs,
        heading_contents,
        code_languages,
        frontmatter_keys,
        title,
        tags,
    })
}
