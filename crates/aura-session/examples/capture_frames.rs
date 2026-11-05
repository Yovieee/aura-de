//! Real PipeWire frame consumer for integration tests.
//!
//! `capture_frames --export` reads AURA_CAPTURE_SOCKET/TOKEN, starts an export,
//! creates an explicit PipeWire link (no session manager required), and checks
//! ten mapped RGBA frames. Alternatively use `--node ID WIDTH HEIGHT` to consume
//! an existing portal node. `--expect-pattern` checks the 2x2 protocol fixture.
#[allow(dead_code)]
#[path = "../src/capture.rs"]
mod capture;

use pipewire::{self as pw, properties::properties, spa};
use std::{
    cell::Cell,
    io,
    rc::Rc,
    time::{Duration, Instant},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "--node-exists") {
        let target = args
            .get(1)
            .ok_or("--node-exists requires a node id")?
            .parse()?;
        println!("node_exists={}", node_exists(target)?);
        return Ok(());
    }
    let expect_pattern = args.iter().any(|arg| arg == "--expect-pattern");
    let export = if args.first().is_some_and(|arg| arg == "--export") {
        let config = capture::CaptureConfig {
            socket: std::env::var_os("AURA_CAPTURE_SOCKET")
                .ok_or("AURA_CAPTURE_SOCKET missing")?
                .into(),
            token: std::env::var("AURA_CAPTURE_TOKEN")?,
            remote: std::env::var("PIPEWIRE_REMOTE").ok(),
        };
        capture::probe(&config)?;
        Some(capture::CaptureStream::start(config)?)
    } else {
        None
    };
    let (target, width, height) = if let Some(export) = &export {
        (export.node_id, export.width, export.height)
    } else if args.first().is_some_and(|arg| arg == "--node") && args.len() >= 4 {
        (args[1].parse()?, args[2].parse()?, args[3].parse()?)
    } else {
        return Err(
            "usage: capture_frames --export [--expect-pattern] | --node ID WIDTH HEIGHT".into(),
        );
    };
    if width == 0 || height == 0 || width > 4096 || height > 4096 {
        return Err("invalid video dimensions".into());
    }
    if let Some(index) = args.iter().position(|arg| arg == "--hold-before-frames") {
        let seconds: u64 = args
            .get(index + 1)
            .ok_or("--hold-before-frames requires seconds")?
            .parse()?;
        if seconds > 30 {
            return Err("hold must not exceed 30 seconds".into());
        }
        println!("export_node={target}");
        std::thread::sleep(Duration::from_secs(seconds));
    }
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, Some(capture::context_properties()))?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry()?;
    let removed = Rc::new(Cell::new(false));
    let removed_callback = removed.clone();
    let _registry_listener = registry
        .add_listener_local()
        .global_remove(move |id| {
            if id == target {
                removed_callback.set(true);
            }
        })
        .register();
    let failed = Rc::new(Cell::new(false));
    let failed_callback = failed.clone();
    let _core_listener = core
        .add_listener_local()
        .error(move |_, _, _, message| {
            eprintln!("PipeWire consumer error: {message}");
            failed_callback.set(true);
        })
        .register();
    let stream = pw::stream::StreamBox::new(
        &core,
        "Aura frame validation",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
            *pw::keys::NODE_NAME => "aura-frame-validation",
        },
    )?;
    let frames = Rc::new(Cell::new(0u32));
    let frames_callback = frames.clone();
    let checksum = Rc::new(Cell::new(0u64));
    let checksum_callback = checksum.clone();
    let invalid = Rc::new(Cell::new(false));
    let invalid_callback = invalid.clone();
    let _listener = stream
        .add_local_listener_with_user_data(())
        .state_changed(|_, _, old, new| eprintln!("consumer state: {old:?} -> {new:?}"))
        .process(move |stream, _| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(data) = buffer.datas_mut().first_mut() else {
                return;
            };
            let offset = data.chunk().offset() as usize;
            let length = data.chunk().size() as usize;
            if length != (width * height * 4) as usize
                || data.chunk().stride() != (width * 4) as i32
            {
                invalid_callback.set(true);
                return;
            }
            let Some(bytes) = data
                .data()
                .and_then(|bytes| bytes.get(offset..offset + length))
            else {
                invalid_callback.set(true);
                return;
            };
            if expect_pattern
                && bytes != [255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 1, 2, 3, 255]
            {
                invalid_callback.set(true);
                return;
            }
            checksum_callback.set(bytes.iter().fold(0u64, |sum, byte| {
                sum.wrapping_mul(31).wrapping_add(*byte as u64)
            }));
            frames_callback.set(frames_callback.get() + 1);
        })
        .register()?;
    let format = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaType,
            Id,
            spa::param::format::MediaType::Video
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaSubtype,
            Id,
            spa::param::format::MediaSubtype::Raw
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Id,
            spa::param::video::VideoFormat::RGBA
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Rectangle,
            spa::utils::Rectangle { width, height }
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Fraction,
            spa::utils::Fraction { num: 30, denom: 1 }
        ),
    );
    let values = spa::pod::serialize::PodSerializer::serialize(
        io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(format),
    )?
    .0
    .into_inner();
    stream.connect(
        spa::utils::Direction::Input,
        None,
        pw::stream::StreamFlags::MAP_BUFFERS,
        &mut [spa::pod::Pod::from_bytes(&values).unwrap()],
    )?;
    let mut link = None;
    let mut link_listener = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    while frames.get() < 10 && !failed.get() && !invalid.get() && Instant::now() < deadline {
        mainloop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(20)));
        if link.is_none()
            && matches!(
                stream.state(),
                pw::stream::StreamState::Paused | pw::stream::StreamState::Streaming
            )
            && stream.node_id() != u32::MAX
        {
            link = Some(core.create_object::<pw::link::Link>(
                "link-factory",
                &properties! {
                    "link.output.node" => target.to_string(),
                    "link.output.port" => "0",
                    "link.input.node" => stream.node_id().to_string(),
                    "link.input.port" => "0",
                },
            )?);
            link_listener = Some(
                link.as_ref()
                    .unwrap()
                    .add_listener_local()
                    .info(|info| eprintln!("consumer link: {:?}", info.state()))
                    .register(),
            );
        }
        if let pw::stream::StreamState::Error(error) = stream.state() {
            return Err(format!("consumer stream: {error}").into());
        }
    }
    if failed.get() || invalid.get() || frames.get() < 10 {
        return Err(format!(
            "frame validation failed: frames={} invalid={} daemon_error={}",
            frames.get(),
            invalid.get(),
            failed.get()
        )
        .into());
    }
    println!(
        "received={} node={} size={}x{} checksum={:016x}",
        frames.get(),
        target,
        width,
        height,
        checksum.get()
    );
    if let Some(export) = &export {
        export.stop();
        let deadline = Instant::now() + Duration::from_secs(2);
        while (!removed.get() || export.is_running()) && Instant::now() < deadline {
            mainloop
                .loop_()
                .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(20)));
        }
        if !removed.get() || export.is_running() {
            return Err("capture stop did not remove the real PipeWire node".into());
        }
        println!("cleanup=removed");
    }
    drop(link_listener);
    drop(link);
    stream.disconnect()?;
    drop(export);
    Ok(())
}

/// A bounded registry roundtrip avoids enumerating every node's parameters.
fn node_exists(target: u32) -> Result<bool, Box<dyn std::error::Error>> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, Some(capture::context_properties()))?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry()?;
    let found = Rc::new(Cell::new(false));
    let found_callback = found.clone();
    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            if global.id == target && global.type_ == pw::types::ObjectType::Node {
                found_callback.set(true);
            }
        })
        .register();
    let pending = core.sync(0)?;
    let done = Rc::new(Cell::new(false));
    let done_callback = done.clone();
    let failed = Rc::new(Cell::new(false));
    let failed_callback = failed.clone();
    let _core_listener = core
        .add_listener_local()
        .done(move |id, sequence| {
            if id == pw::core::PW_ID_CORE && sequence == pending {
                done_callback.set(true);
            }
        })
        .error(move |_, _, _, _| failed_callback.set(true))
        .register();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !done.get() && !failed.get() && Instant::now() < deadline {
        mainloop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(20)));
    }
    if !done.get() || failed.get() {
        return Err("PipeWire registry roundtrip failed".into());
    }
    Ok(found.get())
}
