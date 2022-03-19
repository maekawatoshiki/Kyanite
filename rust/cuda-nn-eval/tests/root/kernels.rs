use bytemuck::{cast_slice, cast_slice_mut};
use itertools::Itertools;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use cuda_nn_eval::tensor::DeviceTensor;
use cuda_nn_eval::{kernels, Device};
use cuda_sys::wrapper::event::CudaEvent;
use cuda_sys::wrapper::handle::CudaStream;
use cuda_sys::wrapper::status::Status;
use nn_graph::ndarray::Array1;

#[test]
fn strided_copy() {
    let device = Device::new(0);
    let stream = CudaStream::new(device);

    let input_data = (0..128).map(|x| x as f32).collect_vec();
    let mut output_data = vec![0f32; 128];

    let input = device.alloc(input_data.len() * 4);
    let output = device.alloc(output_data.len() * 4);

    let rank = 4;
    let size = 56;
    let input_strides: Vec<i32> = vec![64, 8, 0, 2];
    let output_strides: Vec<i32> = vec![24, 8, 4, 1];
    let dense_strides: Vec<i32> = vec![24, 8, 4, 1];

    let start_event = CudaEvent::new();
    let end_event = CudaEvent::new();

    unsafe {
        input.copy_linear_from_host(cast_slice(&input_data));

        stream.record_event(&start_event);

        for _ in 0..128 {
            kernels::stridedCopyFloat(
                stream.inner(),
                rank,
                size,
                input_strides.as_ptr(),
                output_strides.as_ptr(),
                dense_strides.as_ptr(),
                input.ptr() as *const f32,
                output.ptr() as *mut f32,
            )
            .unwrap();
        }
        stream.record_event(&end_event);

        output.copy_linear_to_host(cast_slice_mut(&mut output_data));
    }

    let delta = end_event.time_elapsed_since(&start_event);
    println!("Copy took {}", delta);
    println!("{:?}", output_data);
}

#[test]
fn gather() {
    let device = Device::new(0);
    let stream = CudaStream::new(device);

    let input_data = (0..128).map(|x| x as f32).collect_vec();
    let indices_data: Vec<i32> = vec![16, 3, 8, 2, 4, 9];
    let mut output_data = vec![0f32; indices_data.len()];

    let input = device.alloc(input_data.len() * 4);
    let indices = device.alloc(indices_data.len() * 4);
    let output = device.alloc(output_data.len() * 4);

    unsafe {
        input.copy_linear_from_host(cast_slice(&input_data));
        indices.copy_linear_from_host(cast_slice(&indices_data));

        kernels::gatherFloat(
            stream.inner(),
            indices_data.len() as i32,
            indices.ptr() as *const _,
            input.ptr() as *const _,
            output.ptr() as *mut _,
        )
        .unwrap();

        output.copy_linear_to_host(cast_slice_mut(&mut output_data));
    }

    println!("{:?}", output_data);
    let expected_output_data = indices_data.iter().map(|&x| x as f32).collect_vec();
    assert_eq!(output_data, expected_output_data)
}

#[test]
fn gather_2d_axis1() {
    for batch_size in [0, 1, 2, 3, 4, 8, 13] {
        for input_size in [1, 2, 3, 4, 128, 129, 1000] {
            for index_count in [0, 1, 2, 3, 63, 64, 65, 127, 128, 129, 1000] {
                gather_2d_axis1_impl(batch_size, input_size, index_count);
            }
        }
    }
}

#[test]
fn gather_chess_shape() {
    gather_2d_axis1_impl(128, 4608, 1880);
}

fn gather_2d_axis1_impl(batch_size: usize, input_size: usize, index_count: usize) {
    println!("Testing input: {}x{}, indices: {}", batch_size, input_size, index_count);

    let device = Device::new(0);
    let stream = CudaStream::new(device);

    let input_data: Vec<f32> = (0..batch_size * input_size).map(|x| -(x as f32)).collect_vec();

    let mut index_rng = StdRng::seed_from_u64(1);
    let indices_data: Vec<f32> = (0..index_count)
        .map(|_| index_rng.gen_range(0..input_size) as f32)
        .collect_vec();

    let mut output_data: Vec<f32> = vec![0f32; batch_size * indices_data.len()];

    let input = device.alloc(input_data.len() * 4);
    let indices = device.alloc(indices_data.len() * 4);
    let output = device.alloc(output_data.len() * 4);

    unsafe {
        input.copy_linear_from_host(cast_slice(&input_data));
        indices.copy_linear_from_host(cast_slice(&indices_data));

        let before = stream.record_new_event();

        kernels::gather2dAxis1FloatFloat(
            stream.inner(),
            batch_size as i32,
            input_size as i32,
            input_size as i32,
            1,
            indices_data.len() as i32,
            input.ptr() as *const f32,
            indices.ptr() as *const f32,
            output.ptr() as *mut f32,
        )
        .unwrap();

        let after = stream.record_new_event();
        stream.synchronize();
        println!("Took {}s", after.time_elapsed_since(&before));

        output.copy_linear_to_host(cast_slice_mut(&mut output_data));
    }

    let expected_output_data = (0..batch_size)
        .flat_map(|n| {
            indices_data
                .iter()
                .map(|&i| input_data[n * input_size + i as usize])
                .collect_vec()
        })
        .collect_vec();

    if output_data != expected_output_data {
        eprintln!("{:?}", output_data);
        eprintln!("{:?}", expected_output_data);

        for n in 0..batch_size {
            for q in 0..index_count {
                let index = indices_data[q];
                let i = n * index_count + q;
                let actual = output_data[i];
                let expected = expected_output_data[i];
                if actual != expected {
                    println!(
                        "({}, {}) -> [{}] : actual {}, expected {}",
                        n, q, index, actual, expected
                    )
                }
            }
        }
    }
}

#[test]
fn quantize() {
    let device = Device::new(0);
    let stream = CudaStream::new(device);

    let length = 20;

    let input_data: Vec<f32> = Array1::linspace(-0.2, 1.2, length).to_vec();
    let mut middle_data: Vec<u8> = vec![0; length];
    let mut output_data: Vec<f32> = vec![0.0; length];

    let input = device.alloc(length * 4);
    let middle = device.alloc(length);
    let output = device.alloc(length * 4);

    unsafe {
        input.copy_linear_from_host(cast_slice(&input_data));

        kernels::quantize(
            stream.inner(),
            length as i32,
            input.ptr() as *const _,
            middle.ptr() as *mut _,
        )
        .unwrap();
        kernels::unquantize(
            stream.inner(),
            length as i32,
            middle.ptr() as *const _,
            output.ptr() as *mut _,
        )
        .unwrap();
        stream.synchronize();

        middle.copy_linear_to_host(&mut middle_data);
        output.copy_linear_to_host(cast_slice_mut(&mut output_data));
    }

    println!("{:?}", input_data);
    println!("{:?}", middle_data);
    println!("{:?}", output_data);
}
