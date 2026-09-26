//! Little-endian binary reading with bounds checks.

#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("unexpected end of data at offset {offset} (wanted {wanted} bytes)")]
    Eof { offset: usize, wanted: usize },
    #[error("{0}")]
    Invalid(String),
}

pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8], ReadError> {
        if self.remaining() < n {
            return Err(ReadError::Eof {
                offset: self.pos,
                wanted: n,
            });
        }
        let slice = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    pub fn skip(&mut self, n: usize) -> Result<(), ReadError> {
        self.bytes(n).map(|_| ())
    }

    pub fn u8(&mut self) -> Result<u8, ReadError> {
        Ok(self.bytes(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16, ReadError> {
        let b = self.bytes(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub fn i16(&mut self) -> Result<i16, ReadError> {
        Ok(self.u16()? as i16)
    }

    pub fn u32(&mut self) -> Result<u32, ReadError> {
        let b = self.bytes(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn i32(&mut self) -> Result<i32, ReadError> {
        Ok(self.u32()? as i32)
    }

    pub fn f32(&mut self) -> Result<f32, ReadError> {
        Ok(f32::from_bits(self.u32()?))
    }

    pub fn vec3(&mut self) -> Result<[f32; 3], ReadError> {
        Ok([self.f32()?, self.f32()?, self.f32()?])
    }

    pub fn mat4(&mut self) -> Result<[f32; 16], ReadError> {
        let mut m = [0.0; 16];
        for v in &mut m {
            *v = self.f32()?;
        }
        Ok(m)
    }

    /// A count that is then used to allocate; rejects counts that can't fit in the data.
    pub fn count(&mut self, min_item_size: usize) -> Result<usize, ReadError> {
        let n = self.u32()? as usize;
        if n.saturating_mul(min_item_size) > self.remaining() {
            return Err(ReadError::Invalid(format!(
                "count {n} at offset {} exceeds remaining data",
                self.pos - 4
            )));
        }
        Ok(n)
    }

    /// `u32 length` + bytes (latin-1).
    pub fn string(&mut self) -> Result<String, ReadError> {
        let n = self.count(1)?;
        Ok(self.bytes(n)?.iter().map(|&b| b as char).collect())
    }
}
