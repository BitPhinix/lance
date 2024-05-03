// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

use arrow_array::ArrayRef;
use arrow_schema::DataType;
use bytes::Bytes;
use futures::{future::BoxFuture, FutureExt};
use lance_arrow::DataTypeExt;
use log::trace;
use snafu::{location, Location};
use std::fmt;
use std::ops::Range;
use std::sync::{Arc, Mutex};

use crate::{
    decoder::{PhysicalPageDecoder, PhysicalPageScheduler},
    encoder::{ArrayEncoder, BufferEncoder, EncodedArray, EncodedArrayBuffer, EncodedBuffer},
    format::pb,
    EncodingsIo,
};

use lance_core::{Error, Result};

use super::bitpack::{num_compressed_bits, BitpackingBufferEncoder};
use super::buffers::{
    BitmapBufferEncoder, CompressedBufferEncoder, FlatBufferEncoder, GeneralBufferCompressor,
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CompressionScheme {
    None,
    Zstd,
}

impl fmt::Display for CompressionScheme {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let scheme_str = match self {
            Self::Zstd => "zstd",
            Self::None => "none",
        };
        write!(f, "{}", scheme_str)
    }
}

pub fn parse_compression_scheme(scheme: &str) -> Result<CompressionScheme> {
    match scheme {
        "none" => Ok(CompressionScheme::None),
        "zstd" => Ok(CompressionScheme::Zstd),
        _ => Err(Error::invalid_input(
            format!("Unknown compression scheme: {}", scheme),
            location!(),
        )),
    }
}

/// Scheduler for a simple encoding where buffers of fixed-size items are stored as-is on disk
#[derive(Debug, Clone, Copy)]
pub struct ValuePageScheduler {
    // TODO: do we really support values greater than 2^32 bytes per value?
    // I think we want to, in theory, but will need to test this case.
    bytes_per_value: u64,
    buffer_offset: u64,
    buffer_size: u64,
    compression_scheme: CompressionScheme,
}

impl ValuePageScheduler {
    pub fn new(
        bytes_per_value: u64,
        buffer_offset: u64,
        buffer_size: u64,
        compression_scheme: CompressionScheme,
    ) -> Self {
        Self {
            bytes_per_value,
            buffer_offset,
            buffer_size,
            compression_scheme,
        }
    }
}

impl PhysicalPageScheduler for ValuePageScheduler {
    fn schedule_ranges(
        &self,
        ranges: &[std::ops::Range<u32>],
        scheduler: &dyn EncodingsIo,
        top_level_row: u64,
    ) -> BoxFuture<'static, Result<Box<dyn PhysicalPageDecoder>>> {
        let (mut min, mut max) = (u64::MAX, 0);
        let byte_ranges = if self.compression_scheme == CompressionScheme::None {
            ranges
                .iter()
                .map(|range| {
                    let start = self.buffer_offset + (range.start as u64 * self.bytes_per_value);
                    let end = self.buffer_offset + (range.end as u64 * self.bytes_per_value);
                    min = min.min(start);
                    max = max.max(end);
                    start..end
                })
                .collect::<Vec<_>>()
        } else {
            min = self.buffer_offset;
            max = self.buffer_offset + self.buffer_size;
            // for compressed page, the ranges are always the entire page,
            // and it is guaranteed that only one range is passed
            vec![Range {
                start: min,
                end: max,
            }]
        };

        trace!(
            "Scheduling I/O for {} ranges spread across byte range {}..{}",
            byte_ranges.len(),
            min,
            max
        );
        let bytes = scheduler.submit_request(byte_ranges, top_level_row);
        let bytes_per_value = self.bytes_per_value;

        let range_offsets = if self.compression_scheme != CompressionScheme::None {
            ranges
                .iter()
                .map(|range| {
                    let start = (range.start as u64 * bytes_per_value) as usize;
                    let end = (range.end as u64 * bytes_per_value) as usize;
                    start..end
                })
                .collect::<Vec<_>>()
        } else {
            vec![]
        };

        async move {
            let bytes = bytes.await?;

            Ok(Box::new(ValuePageDecoder {
                bytes_per_value,
                data: bytes,
                uncompressed_data: Arc::new(Mutex::new(None)),
                uncompressed_range_offsets: range_offsets,
            }) as Box<dyn PhysicalPageDecoder>)
        }
        .boxed()
    }
}

struct ValuePageDecoder {
    bytes_per_value: u64,
    data: Vec<Bytes>,
    uncompressed_data: Arc<Mutex<Option<Vec<Bytes>>>>,
    uncompressed_range_offsets: Vec<std::ops::Range<usize>>,
}

impl ValuePageDecoder {
    fn decompress(&self) -> Result<Vec<Bytes>> {
        // for compressed page, it is guaranteed that only one range is passed
        let bytes_u8: Vec<u8> = self.data[0].to_vec();
        let buffer_compressor = GeneralBufferCompressor::get_compressor("");
        let mut uncompressed_bytes: Vec<u8> = Vec::new();
        buffer_compressor.decompress(&bytes_u8, &mut uncompressed_bytes)?;

        let mut bytes_in_ranges: Vec<Bytes> =
            Vec::with_capacity(self.uncompressed_range_offsets.len());
        for range in &self.uncompressed_range_offsets {
            let start = range.start;
            let end = range.end;
            bytes_in_ranges.push(Bytes::from(uncompressed_bytes[start..end].to_vec()));
        }
        Ok(bytes_in_ranges)
    }

    fn get_uncompressed_bytes(&self) -> Result<Arc<Mutex<Option<Vec<Bytes>>>>> {
        let mut uncompressed_bytes = self.uncompressed_data.lock().unwrap();
        if uncompressed_bytes.is_none() {
            *uncompressed_bytes = Some(self.decompress()?);
        }
        Ok(Arc::clone(&self.uncompressed_data))
    }

    fn is_compressed(&self) -> bool {
        !self.uncompressed_range_offsets.is_empty()
    }

    fn decode_buffer(
        &self,
        buf: &Bytes,
        bytes_to_skip: &mut u64,
        bytes_to_take: &mut u64,
        dest: &mut bytes::BytesMut,
    ) {
        let buf_len = buf.len() as u64;
        if *bytes_to_skip > buf_len {
            *bytes_to_skip -= buf_len;
        } else {
            let bytes_to_take_here = (buf_len - *bytes_to_skip).min(*bytes_to_take);
            *bytes_to_take -= bytes_to_take_here;
            let start = *bytes_to_skip as usize;
            let end = start + bytes_to_take_here as usize;
            dest.extend_from_slice(&buf.slice(start..end));
            *bytes_to_skip = 0;
        }
    }
}

impl PhysicalPageDecoder for ValuePageDecoder {
    fn update_capacity(
        &self,
        _rows_to_skip: u32,
        num_rows: u32,
        buffers: &mut [(u64, bool)],
        _all_null: &mut bool,
    ) {
        buffers[0].0 = self.bytes_per_value * num_rows as u64;
        buffers[0].1 = true;
    }

    fn decode_into(
        &self,
        rows_to_skip: u32,
        num_rows: u32,
        dest_buffers: &mut [bytes::BytesMut],
    ) -> Result<()> {
        let mut bytes_to_skip = rows_to_skip as u64 * self.bytes_per_value;
        let mut bytes_to_take = num_rows as u64 * self.bytes_per_value;

        let dest = &mut dest_buffers[0];

        debug_assert!(dest.capacity() as u64 >= bytes_to_take);

        if self.is_compressed() {
            let decoding_data = self.get_uncompressed_bytes()?;
            for buf in decoding_data.lock().unwrap().as_ref().unwrap() {
                self.decode_buffer(buf, &mut bytes_to_skip, &mut bytes_to_take, dest);
            }
        } else {
            for buf in &self.data {
                self.decode_buffer(buf, &mut bytes_to_skip, &mut bytes_to_take, dest);
            }
        }
        Ok(())
    }

    fn num_buffers(&self) -> u32 {
        1
    }
}

#[derive(Debug)]
pub struct ValueEncoder {
    compression_scheme: CompressionScheme,
    flat_buffer_encoder: Box<dyn BufferEncoder>,
    bitpack_buffer_encoder: Option<BitpackingBufferEncoder>,
}

impl ValueEncoder {
    pub fn try_new(data_type: &DataType, compression_scheme: CompressionScheme) -> Result<Self> {
        if *data_type == DataType::Boolean {
            Ok(Self {
                flat_buffer_encoder: Box::<BitmapBufferEncoder>::default(),
                bitpack_buffer_encoder: None,
                compression_scheme,
            })
        } else if data_type.is_fixed_stride() {
            Ok(Self {
                flat_buffer_encoder: if compression_scheme != CompressionScheme::None {
                    Box::<CompressedBufferEncoder>::default()
                } else {
                    Box::<FlatBufferEncoder>::default()
                },
                bitpack_buffer_encoder: Some(BitpackingBufferEncoder::default()),
                compression_scheme,
            })
        } else {
            Err(Error::invalid_input(
                format!("Cannot use ValueEncoder to encode {}", data_type),
                location!(),
            ))
        }
    }

    pub fn try_bitpack_encode(
        &self,
        arrays: &[ArrayRef],
        buffer_index: u32,
    ) -> Result<Option<(pb::array_encoding::ArrayEncoding, EncodedBuffer)>> {
        if self.bitpack_buffer_encoder.is_none() {
            return Ok(None);
        }

        // calculate the number of bits to compress array items into
        let mut num_bits = 0;
        for arr in arrays {
            match num_compressed_bits(arr.clone()) {
                Some(arr_max) => num_bits = num_bits.max(arr_max),
                None => return Ok(None),
            }
        }

        // check that the number of bits in the compressed array is less than the
        // number of bits in the native type. Otherwise there's no point to bitpacking
        let data_type = arrays[0].data_type();
        let native_num_bits = 8 * data_type.byte_width() as u64;
        if num_bits >= native_num_bits {
            return Ok(None);
        }

        let encoded_buffer = self
            .bitpack_buffer_encoder
            .as_ref()
            .unwrap()
            .encode(arrays)?;

        let encoding = pb::array_encoding::ArrayEncoding::Bitpacked(pb::Bitpacked {
            compressed_bits_per_value: num_bits,
            uncompressed_bits_per_value: native_num_bits,
            buffer: Some(pb::Buffer {
                buffer_index,
                buffer_type: pb::buffer::BufferType::Page as i32,
            }),
        });

        Ok(Some((encoding, encoded_buffer)))
    }
}

impl ArrayEncoder for ValueEncoder {
    fn encode(&self, arrays: &[ArrayRef], buffer_index: &mut u32) -> Result<EncodedArray> {
        let index = *buffer_index;
        *buffer_index += 1;

        let bitpack_encoding = self.try_bitpack_encode(arrays, index)?;
        let (array_encoding, encoded_buffer) = match bitpack_encoding {
            Some((array_encoding, encoded_buffer)) => (array_encoding, encoded_buffer),
            None => {
                let data_type = arrays[0].data_type();
                let bits_per_value = match data_type {
                    DataType::Boolean => 1,
                    _ => 8 * data_type.byte_width() as u64,
                };

                let encoded_buffer = self.flat_buffer_encoder.encode(arrays)?;
                let array_encoding = pb::array_encoding::ArrayEncoding::Flat(pb::Flat {
                    bits_per_value,
                    buffer: Some(pb::Buffer {
                        buffer_index: index,
                        buffer_type: pb::buffer::BufferType::Page as i32,
                    }),
                    compression: if self.compression_scheme != CompressionScheme::None {
                        Some(pb::Compression {
                            scheme: self.compression_scheme.to_string(),
                        })
                    } else {
                        None
                    },
                });

                (array_encoding, encoded_buffer)
            }
        };

        let array_bufs = vec![EncodedArrayBuffer {
            parts: encoded_buffer.parts,
            index,
        }];
        let flat_encoding = pb::ArrayEncoding {
            array_encoding: Some(array_encoding),
        };

        Ok(EncodedArray {
            buffers: array_bufs,
            encoding: flat_encoding,
        })
    }
}

// public tests module because we share the PRIMITIVE_TYPES constant with fixed_size_list
#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use std::marker::PhantomData;
    use std::sync::Arc;

    use arrow_array::{
        types::{UInt32Type, UInt64Type, UInt8Type},
        ArrayRef, ArrowPrimitiveType, Float32Array, PrimitiveArray, UInt16Array, UInt32Array,
        UInt64Array, UInt8Array,
    };
    use arrow_schema::{DataType, Field, TimeUnit};
    use rand::distributions::Uniform;

    use lance_datagen::{array::rand_with_distribution, ArrayGenerator};

    use crate::{
        encoder::ArrayEncoder,
        testing::{
            check_round_trip_encoding_generated, check_round_trip_encoding_random,
            ArrayGeneratorProvider,
        },
    };

    const PRIMITIVE_TYPES: &[DataType] = &[
        DataType::FixedSizeBinary(2),
        DataType::Date32,
        DataType::Date64,
        DataType::Int8,
        DataType::Int16,
        DataType::Int32,
        DataType::Int64,
        DataType::UInt8,
        DataType::UInt16,
        DataType::UInt32,
        DataType::UInt64,
        DataType::Float16,
        DataType::Float32,
        DataType::Float64,
        DataType::Decimal128(10, 10),
        DataType::Decimal256(10, 10),
        DataType::Timestamp(TimeUnit::Nanosecond, None),
        DataType::Time32(TimeUnit::Second),
        DataType::Time64(TimeUnit::Nanosecond),
        DataType::Duration(TimeUnit::Second),
        // The Interval type is supported by the reader but the writer works with Lance schema
        // at the moment and Lance schema can't parse interval
        // DataType::Interval(IntervalUnit::DayTime),
    ];

    #[test_log::test(tokio::test)]
    async fn test_value_primitive() {
        for data_type in PRIMITIVE_TYPES {
            let field = Field::new("", data_type.clone(), false);
            check_round_trip_encoding_random(field).await;
        }
    }

    #[test_log::test(test)]
    fn test_will_bitpack_allowed_types_when_possible() {
        let test_cases: Vec<(DataType, ArrayRef, u64)> = vec![
            (
                DataType::UInt8,
                Arc::new(UInt8Array::from_iter_values(vec![0, 1, 2, 3, 4, 5])),
                3, // bits per value
            ),
            (
                DataType::UInt16,
                Arc::new(UInt16Array::from_iter_values(vec![0, 1, 2, 3, 4, 5 << 8])),
                11,
            ),
            (
                DataType::UInt32,
                Arc::new(UInt32Array::from_iter_values(vec![0, 1, 2, 3, 4, 5 << 16])),
                19,
            ),
            (
                DataType::UInt64,
                Arc::new(UInt64Array::from_iter_values(vec![0, 1, 2, 3, 4, 5 << 32])),
                35,
            ),
        ];

        for (data_type, arr, bits_per_value) in test_cases {
            let arrs = vec![arr.clone() as _];
            let mut buffed_index = 1;
            let encoder = ValueEncoder::try_new(&data_type, CompressionScheme::None).unwrap();
            let result = encoder.encode(&arrs, &mut buffed_index).unwrap();
            let array_encoding = result.encoding.array_encoding.unwrap();

            match array_encoding {
                pb::array_encoding::ArrayEncoding::Bitpacked(bitpacked) => {
                    assert_eq!(bits_per_value, bitpacked.compressed_bits_per_value);
                    assert_eq!(
                        (data_type.byte_width() * 8) as u64,
                        bitpacked.uncompressed_bits_per_value
                    );
                }
                _ => {
                    panic!("Array did not use bitpacking encoding")
                }
            }
        }

        // check it will otherwise use flat encoding
        let test_cases: Vec<(DataType, ArrayRef)> = vec![
            // it should use flat encoding for datatypes that don't support bitpacking
            (
                DataType::Float32,
                Arc::new(Float32Array::from_iter_values(vec![0.1, 0.2, 0.3])),
            ),
            // it should still use flat encoding if bitpacked encoding would be packed
            // into the full byte range
            (
                DataType::UInt8,
                Arc::new(UInt8Array::from_iter_values(vec![0, 1, 2, 3, 4, 250])),
            ),
            (
                DataType::UInt16,
                Arc::new(UInt16Array::from_iter_values(vec![0, 1, 2, 3, 4, 250 << 8])),
            ),
            (
                DataType::UInt32,
                Arc::new(UInt32Array::from_iter_values(vec![
                    0,
                    1,
                    2,
                    3,
                    4,
                    250 << 24,
                ])),
            ),
            (
                DataType::UInt64,
                Arc::new(UInt64Array::from_iter_values(vec![
                    0,
                    1,
                    2,
                    3,
                    4,
                    250 << 56,
                ])),
            ),
        ];

        for (data_type, arr) in test_cases {
            let arrs = vec![arr.clone() as _];
            let mut buffed_index = 1;
            let encoder = ValueEncoder::try_new(&data_type, CompressionScheme::None).unwrap();
            let result = encoder.encode(&arrs, &mut buffed_index).unwrap();
            let array_encoding = result.encoding.array_encoding.unwrap();

            match array_encoding {
                pb::array_encoding::ArrayEncoding::Flat(flat) => {
                    assert_eq!((data_type.byte_width() * 8) as u64, flat.bits_per_value);
                }
                _ => {
                    panic!("Array did not use bitpacking encoding")
                }
            }
        }
    }

    struct DistributionArrayGeneratorProvider<
        DataType,
        Dist: rand::distributions::Distribution<DataType::Native> + Clone + Send + Sync + 'static,
    >
    where
        DataType::Native: Copy + 'static,
        PrimitiveArray<DataType>: From<Vec<DataType::Native>> + 'static,
        DataType: ArrowPrimitiveType,
    {
        phantom: PhantomData<DataType>,
        distribution: Dist,
    }

    impl<DataType, Dist> DistributionArrayGeneratorProvider<DataType, Dist>
    where
        Dist: rand::distributions::Distribution<DataType::Native> + Clone + Send + Sync + 'static,
        DataType::Native: Copy + 'static,
        PrimitiveArray<DataType>: From<Vec<DataType::Native>> + 'static,
        DataType: ArrowPrimitiveType,
    {
        fn new(dist: Dist) -> Self {
            DistributionArrayGeneratorProvider::<DataType, Dist> {
                distribution: dist,
                phantom: Default::default(),
            }
        }
    }

    impl<DataType, Dist> ArrayGeneratorProvider for DistributionArrayGeneratorProvider<DataType, Dist>
    where
        Dist: rand::distributions::Distribution<DataType::Native> + Clone + Send + Sync + 'static,
        DataType::Native: Copy + 'static,
        PrimitiveArray<DataType>: From<Vec<DataType::Native>> + 'static,
        DataType: ArrowPrimitiveType,
    {
        fn provide(&self) -> Box<dyn ArrayGenerator> {
            let generator = rand_with_distribution::<DataType, Dist>(self.distribution.clone());
            generator
        }

        fn copy(&self) -> Box<dyn ArrayGeneratorProvider> {
            Box::new(DistributionArrayGeneratorProvider::<DataType, Dist> {
                phantom: self.phantom.clone(),
                distribution: self.distribution.clone(),
            })
        }
    }

    #[test_log::test(tokio::test)]
    async fn test_bitpack_primitive() {
        let bitpacked_test_cases: &Vec<(DataType, Box<dyn ArrayGeneratorProvider>)> = &vec![
            // check less than one byte for multi-byte type
            (
                DataType::UInt32,
                Box::new(
                    DistributionArrayGeneratorProvider::<UInt32Type, Uniform<u32>>::new(
                        Uniform::new(0, 19),
                    ),
                ),
            ),
            // check that more than one byte for multi-byte type
            (
                DataType::UInt32,
                Box::new(
                    DistributionArrayGeneratorProvider::<UInt32Type, Uniform<u32>>::new(
                        Uniform::new(5 << 7, 6 << 7),
                    ),
                ),
            ),
            (
                DataType::UInt64,
                Box::new(
                    DistributionArrayGeneratorProvider::<UInt64Type, Uniform<u64>>::new(
                        Uniform::new(5 << 42, 6 << 42),
                    ),
                ),
            ),
            // check less than one byte for single-byte type
            (
                DataType::UInt8,
                Box::new(
                    DistributionArrayGeneratorProvider::<UInt8Type, Uniform<u8>>::new(
                        Uniform::new(0, 19),
                    ),
                ),
            ),
            // check byte aligned for single byte
            (
                DataType::UInt32,
                Box::new(
                    DistributionArrayGeneratorProvider::<UInt32Type, Uniform<u32>>::new(
                        // this range should always give 8 bits
                        Uniform::new(200, 250),
                    ),
                ),
            ),
            // check byte aligned for multiple bytes
            (
                DataType::UInt32,
                Box::new(
                    DistributionArrayGeneratorProvider::<UInt32Type, Uniform<u32>>::new(
                        // this range should always always give 16 bits
                        Uniform::new(200 << 8, 250 << 8),
                    ),
                ),
            ),
            // check that we can still encode an all-0 array
            (
                DataType::UInt32,
                Box::new(
                    DistributionArrayGeneratorProvider::<UInt32Type, Uniform<u32>>::new(
                        // this range should always always give 16 bits
                        Uniform::new(0, 1),
                    ),
                ),
            ),
        ];

        for (data_type, array_gen_provider) in bitpacked_test_cases {
            let field = Field::new("", data_type.clone(), false);
            check_round_trip_encoding_generated(field, array_gen_provider.copy()).await;
        }
    }
}
