//! Integration tests for NDArray, NDArrayPool, and NDAttributeList.
//!
//! Ported from C ADCore test_NDArrayPool.cpp patterns, exercising:
//! - NDArray creation with different data types
//! - NDArray dimension handling (1D, 2D, 3D)
//! - NDArrayPool allocation and free-list reuse
//! - NDArrayPool memory limit enforcement
//! - NDArray data buffer read/write
//! - NDAttribute list management
//! - NDArray copy/clone behavior

use std::sync::Arc;

use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute, NDAttributeList};
use ad_core_rs::ndarray::{NDArray, NDDataBuffer, NDDataType, NDDimension};
use ad_core_rs::ndarray_pool::NDArrayPool;

// ---------------------------------------------------------------------------
// NDArray creation with different data types
// ---------------------------------------------------------------------------

#[test]
fn create_int8_array() {
    let arr = NDArray::new(vec![NDDimension::new(64)], NDDataType::Int8);
    assert_eq!(arr.data.data_type(), NDDataType::Int8);
    assert_eq!(arr.data.len(), 64);
    assert_eq!(arr.data.total_bytes(), 64);
    arr.validate().unwrap();
}

#[test]
fn create_int16_array() {
    let arr = NDArray::new(vec![NDDimension::new(128)], NDDataType::Int16);
    assert_eq!(arr.data.data_type(), NDDataType::Int16);
    assert_eq!(arr.data.len(), 128);
    assert_eq!(arr.data.total_bytes(), 256);
    arr.validate().unwrap();
}

#[test]
fn create_int32_array() {
    let arr = NDArray::new(vec![NDDimension::new(256)], NDDataType::Int32);
    assert_eq!(arr.data.data_type(), NDDataType::Int32);
    assert_eq!(arr.data.len(), 256);
    assert_eq!(arr.data.total_bytes(), 1024);
    arr.validate().unwrap();
}

#[test]
fn create_float32_array() {
    let arr = NDArray::new(vec![NDDimension::new(100)], NDDataType::Float32);
    assert_eq!(arr.data.data_type(), NDDataType::Float32);
    assert_eq!(arr.data.len(), 100);
    assert_eq!(arr.data.total_bytes(), 400);
    arr.validate().unwrap();
}

#[test]
fn create_float64_array() {
    let arr = NDArray::new(vec![NDDimension::new(50)], NDDataType::Float64);
    assert_eq!(arr.data.data_type(), NDDataType::Float64);
    assert_eq!(arr.data.len(), 50);
    assert_eq!(arr.data.total_bytes(), 400);
    arr.validate().unwrap();
}

#[test]
fn create_all_unsigned_types() {
    for (dt, elem_size) in [
        (NDDataType::UInt8, 1),
        (NDDataType::UInt16, 2),
        (NDDataType::UInt32, 4),
        (NDDataType::UInt64, 8),
    ] {
        let arr = NDArray::new(vec![NDDimension::new(10)], dt);
        assert_eq!(arr.data.data_type(), dt);
        assert_eq!(arr.data.total_bytes(), 10 * elem_size);
        arr.validate().unwrap();
    }
}

// ---------------------------------------------------------------------------
// NDArray dimension handling
// ---------------------------------------------------------------------------

#[test]
fn dimension_1d() {
    let dims = vec![NDDimension::new(1024)];
    let arr = NDArray::new(dims, NDDataType::UInt8);
    let info = arr.info();
    assert_eq!(info.x_size, 1024);
    // C parity: 1-D arrays leave ySize / colorSize at 0.
    assert_eq!(info.y_size, 0);
    assert_eq!(info.color_size, 0);
    assert_eq!(info.num_elements, 1024);
    assert_eq!(info.bytes_per_element, 1);
}

#[test]
fn dimension_2d_mono() {
    let dims = vec![NDDimension::new(640), NDDimension::new(480)];
    let arr = NDArray::new(dims, NDDataType::UInt16);
    let info = arr.info();
    assert_eq!(info.x_size, 640);
    assert_eq!(info.y_size, 480);
    // C parity: 2-D arrays leave colorSize at 0 (no color dimension).
    assert_eq!(info.color_size, 0);
    assert_eq!(info.num_elements, 640 * 480);
    assert_eq!(info.bytes_per_element, 2);
    assert_eq!(info.total_bytes, 640 * 480 * 2);
}

#[test]
fn dimension_3d_rgb() {
    // NDColorMode convention: dim[0]=color, dim[1]=x, dim[2]=y for RGB1
    let dims = vec![
        NDDimension::new(3),
        NDDimension::new(320),
        NDDimension::new(240),
    ];
    let mut arr = NDArray::new(dims, NDDataType::UInt8);
    // info() reads ColorMode attribute for 3D arrays
    use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
    arr.attributes.add(NDAttribute {
        name: "ColorMode".into(),
        description: "Color Mode".into(),
        source: NDAttrSource::Driver,
        value: NDAttrValue::Int32(2), // RGB1
        source_impl: None,
    });
    let info = arr.info();
    assert_eq!(info.color_size, 3);
    assert_eq!(info.x_size, 320);
    assert_eq!(info.y_size, 240);
    assert_eq!(info.num_elements, 3 * 320 * 240);
}

#[test]
fn dimension_properties() {
    let mut dim = NDDimension::new(512);
    assert_eq!(dim.size, 512);
    assert_eq!(dim.offset, 0);
    assert_eq!(dim.binning, 1);
    assert!(!dim.reverse);

    dim.offset = 10;
    dim.binning = 2;
    dim.reverse = true;
    assert_eq!(dim.offset, 10);
    assert_eq!(dim.binning, 2);
    assert!(dim.reverse);
}

// ---------------------------------------------------------------------------
// NDArrayPool allocation and free-list reuse
// ---------------------------------------------------------------------------

#[test]
fn pool_basic_alloc() {
    let pool = NDArrayPool::new(1_000_000);
    let arr = pool
        .alloc(vec![NDDimension::new(100)], NDDataType::UInt8)
        .unwrap();
    assert_eq!(arr.unique_id, 1);
    assert_eq!(arr.data.len(), 100);
    assert_eq!(pool.allocated_bytes(), 100);
    assert_eq!(pool.num_alloc_buffers(), 1);
}

#[test]
fn pool_sequential_unique_ids() {
    let pool = NDArrayPool::new(1_000_000);
    let a1 = pool
        .alloc(vec![NDDimension::new(10)], NDDataType::UInt8)
        .unwrap();
    let a2 = pool
        .alloc(vec![NDDimension::new(10)], NDDataType::UInt8)
        .unwrap();
    let a3 = pool
        .alloc(vec![NDDimension::new(10)], NDDataType::UInt8)
        .unwrap();
    assert_eq!(a1.unique_id, 1);
    assert_eq!(a2.unique_id, 2);
    assert_eq!(a3.unique_id, 3);
}

#[test]
fn pool_free_list_reuse() {
    let pool = NDArrayPool::new(1_000_000);

    // Allocate and release
    let arr = pool
        .alloc(vec![NDDimension::new(100)], NDDataType::UInt8)
        .unwrap();
    // Accounting tracks the exact requested data_size (C parity), not Vec capacity.
    assert_eq!(pool.allocated_bytes(), 100);
    pool.release(arr);
    assert_eq!(pool.num_free_buffers(), 1);

    // Allocate again — use size within THRESHOLD_SIZE_RATIO (1.5x) for reuse.
    let arr2 = pool
        .alloc(vec![NDDimension::new(80)], NDDataType::UInt8)
        .unwrap();
    assert_eq!(pool.num_free_buffers(), 0);
    // C parity: NDArrayPool::alloc skips the realloc block when the existing
    // buffer is already large enough, so `dataSize` and `memorySize_` stay at
    // the buffer's original size. Reusing a 100-byte buffer for 80 bytes keeps
    // the tracked data_size at 100; only the live element count is 80.
    assert_eq!(pool.allocated_bytes(), 100);
    assert_eq!(arr2.data.len(), 80);
    assert_eq!(arr2.data_size, 100);
    // unique_id keeps incrementing even on reuse
    assert_eq!(arr2.unique_id, 2);
}

#[test]
fn pool_free_list_prefers_smallest_sufficient_buffer() {
    let pool = NDArrayPool::new(10_000_000);

    let small = pool
        .alloc(vec![NDDimension::new(100)], NDDataType::UInt8)
        .unwrap();
    let large = pool
        .alloc(vec![NDDimension::new(10000)], NDDataType::UInt8)
        .unwrap();
    let medium = pool
        .alloc(vec![NDDimension::new(1000)], NDDataType::UInt8)
        .unwrap();

    pool.release(large);
    pool.release(medium);
    pool.release(small);
    assert_eq!(pool.num_free_buffers(), 3);

    // Request 800 bytes — within 1.5x of medium (1000), should pick medium, not large
    let reused = pool
        .alloc(vec![NDDimension::new(800)], NDDataType::UInt8)
        .unwrap();
    assert_eq!(pool.num_free_buffers(), 2);
    assert!(reused.data.capacity_bytes() >= 1000);
}

#[test]
fn pool_empty_free_list() {
    let pool = NDArrayPool::new(1_000_000);
    let a1 = pool
        .alloc(vec![NDDimension::new(100)], NDDataType::UInt8)
        .unwrap();
    let a2 = pool
        .alloc(vec![NDDimension::new(200)], NDDataType::UInt8)
        .unwrap();
    pool.release(a1);
    pool.release(a2);
    assert_eq!(pool.num_free_buffers(), 2);

    pool.empty_free_list();
    assert_eq!(pool.num_free_buffers(), 0);
    assert_eq!(pool.num_alloc_buffers(), 0);
}

// ---------------------------------------------------------------------------
// NDArrayPool memory limit enforcement
// ---------------------------------------------------------------------------

#[test]
fn pool_memory_limit_rejects_oversized_alloc() {
    let pool = NDArrayPool::new(100);
    let result = pool.alloc(vec![NDDimension::new(200)], NDDataType::UInt8);
    assert!(result.is_err());
}

#[test]
fn pool_memory_limit_rejects_cumulative_overflow() {
    let pool = NDArrayPool::new(500);
    let _a1 = pool
        .alloc(vec![NDDimension::new(400)], NDDataType::UInt8)
        .unwrap();
    // 400 already allocated, 200 more would exceed 500
    let result = pool.alloc(vec![NDDimension::new(200)], NDDataType::UInt8);
    assert!(result.is_err());
}

#[test]
fn pool_max_memory_accessor() {
    let pool = NDArrayPool::new(42_000);
    assert_eq!(pool.max_memory(), 42_000);
}

#[test]
fn pool_alloc_copy_respects_memory_limit() {
    let pool = NDArrayPool::new(60);
    let source = pool
        .alloc(vec![NDDimension::new(50)], NDDataType::UInt8)
        .unwrap();
    // 50 bytes already allocated; copying 50 more would exceed 60
    assert!(pool.alloc_copy(&source).is_err());
}

// ---------------------------------------------------------------------------
// NDArray data buffer read/write
// ---------------------------------------------------------------------------

#[test]
fn buffer_get_set_f64_roundtrip() {
    let mut buf = NDDataBuffer::zeros(NDDataType::Float64, 10);
    buf.set_from_f64(0, 3.15);
    buf.set_from_f64(9, -2.72);
    assert_eq!(buf.get_as_f64(0), Some(3.15));
    assert_eq!(buf.get_as_f64(9), Some(-2.72));
    // Unset elements are zero
    assert_eq!(buf.get_as_f64(5), Some(0.0));
}

#[test]
fn buffer_get_set_integer_types() {
    let mut buf = NDDataBuffer::zeros(NDDataType::Int32, 5);
    buf.set_from_f64(0, 42.0);
    buf.set_from_f64(1, -7.0);
    assert_eq!(buf.get_as_f64(0), Some(42.0));
    assert_eq!(buf.get_as_f64(1), Some(-7.0));

    let mut buf16 = NDDataBuffer::zeros(NDDataType::Int16, 5);
    buf16.set_from_f64(0, 1000.0);
    assert_eq!(buf16.get_as_f64(0), Some(1000.0));
}

#[test]
fn buffer_get_out_of_bounds_returns_none() {
    let buf = NDDataBuffer::zeros(NDDataType::UInt8, 10);
    assert!(buf.get_as_f64(10).is_none());
    assert!(buf.get_as_f64(100).is_none());
}

#[test]
fn buffer_set_out_of_bounds_is_noop() {
    let mut buf = NDDataBuffer::zeros(NDDataType::UInt8, 3);
    buf.set_from_f64(100, 42.0); // should not panic
    assert_eq!(buf.len(), 3);
}

#[test]
fn buffer_resize_grow_and_shrink() {
    let mut buf = NDDataBuffer::zeros(NDDataType::Float32, 10);
    buf.set_from_f64(0, 1.0);
    assert_eq!(buf.len(), 10);

    buf.resize(20);
    assert_eq!(buf.len(), 20);
    // Original data preserved
    assert_eq!(buf.get_as_f64(0), Some(1.0));
    // New elements are zero
    assert_eq!(buf.get_as_f64(15), Some(0.0));

    buf.resize(5);
    assert_eq!(buf.len(), 5);
    assert_eq!(buf.get_as_f64(0), Some(1.0));
}

#[test]
fn buffer_as_u8_slice_length() {
    let buf = NDDataBuffer::zeros(NDDataType::UInt16, 100);
    assert_eq!(buf.as_u8_slice().len(), 200);

    let buf32 = NDDataBuffer::zeros(NDDataType::Float32, 50);
    assert_eq!(buf32.as_u8_slice().len(), 200);

    let buf64 = NDDataBuffer::zeros(NDDataType::Float64, 25);
    assert_eq!(buf64.as_u8_slice().len(), 200);
}

#[test]
fn buffer_is_empty() {
    let buf = NDDataBuffer::zeros(NDDataType::UInt8, 0);
    assert!(buf.is_empty());

    let buf2 = NDDataBuffer::zeros(NDDataType::UInt8, 1);
    assert!(!buf2.is_empty());
}

#[test]
fn buffer_direct_vec_access() {
    let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
    if let NDDataBuffer::U8(ref mut v) = arr.data {
        v[0] = 10;
        v[1] = 20;
        v[2] = 30;
        v[3] = 40;
    }
    assert_eq!(arr.data.get_as_f64(0), Some(10.0));
    assert_eq!(arr.data.get_as_f64(3), Some(40.0));
    assert_eq!(arr.data.as_u8_slice(), &[10, 20, 30, 40]);
}

// ---------------------------------------------------------------------------
// NDAttribute list management
// ---------------------------------------------------------------------------

#[test]
fn attribute_list_add_and_find() {
    let mut list = NDAttributeList::new();
    assert!(list.is_empty());

    list.add(NDAttribute {
        name: "ColorMode".into(),
        description: "Color mode of the image".into(),
        source: NDAttrSource::Driver,
        value: NDAttrValue::Int32(0),
        source_impl: None,
    });

    assert_eq!(list.len(), 1);
    let attr = list.get("ColorMode").unwrap();
    assert_eq!(attr.value, NDAttrValue::Int32(0));
    assert_eq!(attr.description, "Color mode of the image");
}

#[test]
fn attribute_list_replace_existing() {
    let mut list = NDAttributeList::new();
    list.add(NDAttribute {
        name: "Gain".into(),
        description: "".into(),
        source: NDAttrSource::Driver,
        value: NDAttrValue::Float64(1.0),
        source_impl: None,
    });
    list.add(NDAttribute {
        name: "Gain".into(),
        description: "Updated".into(),
        source: NDAttrSource::Driver,
        value: NDAttrValue::Float64(2.5),
        source_impl: None,
    });
    // Should not duplicate
    assert_eq!(list.len(), 1);
    assert_eq!(list.get("Gain").unwrap().value, NDAttrValue::Float64(2.5));
    assert_eq!(list.get("Gain").unwrap().description, "Updated");
}

#[test]
fn attribute_list_get_missing() {
    let list = NDAttributeList::new();
    assert!(list.get("nonexistent").is_none());
}

#[test]
fn attribute_list_remove() {
    let mut list = NDAttributeList::new();
    list.add(NDAttribute {
        name: "Temp".into(),
        description: "".into(),
        source: NDAttrSource::Driver,
        value: NDAttrValue::Float64(25.0),
        source_impl: None,
    });
    assert!(list.remove("Temp"));
    assert!(list.is_empty());
    assert!(!list.remove("Temp")); // already removed
}

#[test]
fn attribute_list_clear() {
    let mut list = NDAttributeList::new();
    for i in 0..5 {
        list.add(NDAttribute {
            name: format!("attr_{i}"),
            description: "".into(),
            source: NDAttrSource::Constant(String::new()),
            value: NDAttrValue::Int32(i),
            source_impl: None,
        });
    }
    assert_eq!(list.len(), 5);
    list.clear();
    assert!(list.is_empty());
}

#[test]
fn attribute_list_iter() {
    let mut list = NDAttributeList::new();
    list.add(NDAttribute {
        name: "A".into(),
        description: "".into(),
        source: NDAttrSource::Constant(String::new()),
        value: NDAttrValue::Int32(1),
        source_impl: None,
    });
    list.add(NDAttribute {
        name: "B".into(),
        description: "".into(),
        source: NDAttrSource::Constant(String::new()),
        value: NDAttrValue::String("hello".into()),
        source_impl: None,
    });
    let names: Vec<_> = list.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, vec!["A", "B"]);
}

#[test]
fn attribute_value_conversions() {
    let v = NDAttrValue::Int32(42);
    assert_eq!(v.as_f64(), Some(42.0));
    assert_eq!(v.as_i64(), Some(42));
    assert_eq!(v.as_string(), "42");

    let s = NDAttrValue::String("hello".into());
    assert_eq!(s.as_f64(), None);
    assert_eq!(s.as_i64(), None);
    assert_eq!(s.as_string(), "hello");
}

#[test]
fn attribute_source_types() {
    let driver_attr = NDAttribute {
        name: "driver_val".into(),
        description: "".into(),
        source: NDAttrSource::Driver,
        value: NDAttrValue::Int32(1),
        source_impl: None,
    };
    assert_eq!(driver_attr.source, NDAttrSource::Driver);

    let param_attr = NDAttribute {
        name: "param_val".into(),
        description: "".into(),
        source: NDAttrSource::Param {
            port_name: "SIM1".into(),
            param_name: "TEMPERATURE".into(),
        },
        value: NDAttrValue::Float64(25.0),
        source_impl: None,
    };
    match &param_attr.source {
        NDAttrSource::Param {
            port_name,
            param_name,
        } => {
            assert_eq!(port_name, "SIM1");
            assert_eq!(param_name, "TEMPERATURE");
        }
        _ => panic!("expected Param source"),
    }

    let const_attr = NDAttribute {
        name: "const_val".into(),
        description: "".into(),
        source: NDAttrSource::Constant(String::new()),
        value: NDAttrValue::UInt8(255),
        source_impl: None,
    };
    assert_eq!(const_attr.source, NDAttrSource::Constant(String::new()));
}

// ---------------------------------------------------------------------------
// NDArray copy/clone behavior
// ---------------------------------------------------------------------------

#[test]
fn ndarray_clone_is_independent() {
    let mut arr = NDArray::new(
        vec![NDDimension::new(10), NDDimension::new(10)],
        NDDataType::Float64,
    );
    arr.unique_id = 42;
    arr.data.set_from_f64(0, 99.0);

    let mut cloned = arr.clone();
    assert_eq!(cloned.unique_id, 42);
    assert_eq!(cloned.data.get_as_f64(0), Some(99.0));
    assert_eq!(cloned.dims.len(), 2);

    // Mutating clone does not affect original
    cloned.data.set_from_f64(0, 0.0);
    cloned.unique_id = 100;
    assert_eq!(arr.data.get_as_f64(0), Some(99.0));
    assert_eq!(arr.unique_id, 42);
}

#[test]
fn pool_alloc_copy_preserves_data_and_assigns_new_id() {
    let pool = NDArrayPool::new(1_000_000);
    let mut source = pool
        .alloc(vec![NDDimension::new(4)], NDDataType::UInt8)
        .unwrap();
    if let NDDataBuffer::U8(ref mut v) = source.data {
        v[0] = 1;
        v[1] = 2;
        v[2] = 3;
        v[3] = 4;
    }

    let copy = pool.alloc_copy(&source).unwrap();
    assert_ne!(copy.unique_id, source.unique_id);
    assert_eq!(copy.dims.len(), source.dims.len());
    if let NDDataBuffer::U8(ref v) = copy.data {
        assert_eq!(v, &[1, 2, 3, 4]);
    } else {
        panic!("wrong data type in copy");
    }
}

#[test]
fn pool_alloc_copy_tracks_memory() {
    let pool = NDArrayPool::new(1_000_000);
    let source = pool
        .alloc(vec![NDDimension::new(10)], NDDataType::UInt16)
        .unwrap();
    assert_eq!(pool.allocated_bytes(), 20);
    let _copy = pool.alloc_copy(&source).unwrap();
    assert_eq!(pool.allocated_bytes(), 40);
}

// ---------------------------------------------------------------------------
// NDArray validate
// ---------------------------------------------------------------------------

#[test]
fn validate_ok_for_matching_dims() {
    let arr = NDArray::new(
        vec![NDDimension::new(10), NDDimension::new(20)],
        NDDataType::Float64,
    );
    arr.validate().unwrap();
}

#[test]
fn validate_fails_for_mismatched_buffer() {
    let mut arr = NDArray::new(
        vec![NDDimension::new(10), NDDimension::new(20)],
        NDDataType::UInt8,
    );
    // Replace buffer with wrong size
    arr.data = NDDataBuffer::U8(vec![0; 100]);
    assert!(arr.validate().is_err());
}

// ---------------------------------------------------------------------------
// NDArrayPool concurrent access
// ---------------------------------------------------------------------------

#[test]
fn pool_concurrent_alloc_release() {
    use std::thread;

    let pool = Arc::new(NDArrayPool::new(10_000_000));
    let mut handles = Vec::new();

    for _ in 0..4 {
        let pool = pool.clone();
        handles.push(thread::spawn(move || {
            for _ in 0..100 {
                let arr = pool
                    .alloc(vec![NDDimension::new(100)], NDDataType::UInt8)
                    .unwrap();
                pool.release(arr);
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    // All should have been released back
    assert!(pool.num_free_buffers() > 0);
}

// ---------------------------------------------------------------------------
// NDArrayPool: reuse with different data type
// ---------------------------------------------------------------------------

#[test]
fn pool_reuse_with_different_data_type() {
    let pool = NDArrayPool::new(10_000_000);

    // Allocate UInt8 array with 1000 elements (1000 bytes)
    let arr = pool
        .alloc(vec![NDDimension::new(1000)], NDDataType::UInt8)
        .unwrap();
    pool.release(arr);
    assert_eq!(pool.num_free_buffers(), 1);

    // Now request a Float32 array with 100 elements (400 bytes) — should reuse
    // even though the data type is different (capacity 1000 >= 400 needed)
    let arr2 = pool
        .alloc(vec![NDDimension::new(100)], NDDataType::Float32)
        .unwrap();
    assert_eq!(arr2.data.data_type(), NDDataType::Float32);
    assert_eq!(arr2.data.len(), 100);
    arr2.validate().unwrap();
}

// ---------------------------------------------------------------------------
// NDArray attributes are cleared on pool reuse
// ---------------------------------------------------------------------------

#[test]
fn pool_reuse_clears_attributes() {
    let pool = NDArrayPool::new(1_000_000);
    let mut arr = pool
        .alloc(vec![NDDimension::new(100)], NDDataType::UInt8)
        .unwrap();

    arr.attributes.add(NDAttribute {
        name: "test".into(),
        description: "".into(),
        source: NDAttrSource::Driver,
        value: NDAttrValue::Int32(1),
        source_impl: None,
    });
    assert_eq!(arr.attributes.len(), 1);

    pool.release(arr);

    // Re-allocate — attributes should be cleared
    let arr2 = pool
        .alloc(vec![NDDimension::new(50)], NDDataType::UInt8)
        .unwrap();
    assert!(arr2.attributes.is_empty());
}

// ---------------------------------------------------------------------------
// NDDataType element_size and from_ordinal
// ---------------------------------------------------------------------------

#[test]
fn data_type_element_sizes() {
    assert_eq!(NDDataType::Int8.element_size(), 1);
    assert_eq!(NDDataType::UInt8.element_size(), 1);
    assert_eq!(NDDataType::Int16.element_size(), 2);
    assert_eq!(NDDataType::UInt16.element_size(), 2);
    assert_eq!(NDDataType::Int32.element_size(), 4);
    assert_eq!(NDDataType::UInt32.element_size(), 4);
    assert_eq!(NDDataType::Int64.element_size(), 8);
    assert_eq!(NDDataType::UInt64.element_size(), 8);
    assert_eq!(NDDataType::Float32.element_size(), 4);
    assert_eq!(NDDataType::Float64.element_size(), 8);
}

#[test]
fn data_type_from_ordinal_roundtrip() {
    for i in 0..10u8 {
        let dt = NDDataType::from_ordinal(i).unwrap();
        assert_eq!(dt as u8, i);
    }
    assert!(NDDataType::from_ordinal(10).is_none());
    assert!(NDDataType::from_ordinal(255).is_none());
}

// ---------------------------------------------------------------------------
// NDArray with attributes on array itself
// ---------------------------------------------------------------------------

#[test]
fn ndarray_attributes_on_instance() {
    let mut arr = NDArray::new(vec![NDDimension::new(100)], NDDataType::UInt16);

    arr.attributes.add(NDAttribute {
        name: "ColorMode".into(),
        description: "".into(),
        source: NDAttrSource::Driver,
        value: NDAttrValue::Int32(0),
        source_impl: None,
    });
    arr.attributes.add(NDAttribute {
        name: "Gain".into(),
        description: "".into(),
        source: NDAttrSource::Driver,
        value: NDAttrValue::Float64(1.5),
        source_impl: None,
    });

    assert_eq!(arr.attributes.len(), 2);
    assert_eq!(
        arr.attributes.get("ColorMode").unwrap().value,
        NDAttrValue::Int32(0)
    );
    assert_eq!(
        arr.attributes.get("Gain").unwrap().value,
        NDAttrValue::Float64(1.5)
    );
}

// ---------------------------------------------------------------------------
// NDArray data buffer capacity_bytes
// ---------------------------------------------------------------------------

#[test]
fn buffer_capacity_bytes() {
    let buf = NDDataBuffer::zeros(NDDataType::Float64, 100);
    // capacity should be at least len * element_size
    assert!(buf.capacity_bytes() >= 800);
}
