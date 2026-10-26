use crate::config::Config;
use clap::Args;
use input_emulation::{InputEmulation, InputEmulationError};
use input_event::{Event, KeyboardEvent, PointerEvent};
use std::f64::consts::PI;
use std::time::{Duration, Instant};

const FREQUENCY_HZ: f64 = 1.0;
const RADIUS: f64 = 100.0;

#[derive(Args, Clone, Debug, Eq, PartialEq)]
pub struct TestEmulationArgs {
    #[arg(long)]
    mouse: bool,
    #[arg(long)]
    keyboard: bool,
    #[arg(long)]
    scroll: bool,
}

pub async fn run(config: Config, args: TestEmulationArgs) -> Result<(), InputEmulationError> {
    log::info!("running input emulation test");

    let backend = config.emulation_backend().map(|b| b.into());
    let mut emulation = InputEmulation::new(backend).await?;
    emulation.create(0).await;

    let start = Instant::now();
    let mut offset = (0, 0);
    // `--keyboard`: one A press/release every two seconds. `key` is the
    // Linux evdev code; each sink applies its own keycode translation.
    let mut key_state = 0u8;
    let mut key_next = Instant::now();
    loop {
        tokio::time::sleep(Duration::from_millis(1)).await;
        let elapsed = start.elapsed();
        let elapsed_sec_f64 = elapsed.as_secs_f64();
        let second_fraction = elapsed_sec_f64 - elapsed_sec_f64 as u64 as f64;
        let radians = second_fraction * 2. * PI * FREQUENCY_HZ;
        let new_offset_f = (radians.cos() * RADIUS * 2., (radians * 2.).sin() * RADIUS);
        let new_offset = (new_offset_f.0 as i32, new_offset_f.1 as i32);
        if new_offset != offset {
            let relative_motion = (new_offset.0 - offset.0, new_offset.1 - offset.1);
            offset = new_offset;
            let (dx, dy) = (relative_motion.0 as f64, relative_motion.1 as f64);
            let event = Event::Pointer(PointerEvent::Motion { time: 0, dx, dy });
            emulation.consume(event, 0).await?;
        }
        if args.keyboard {
            let now = Instant::now();
            if key_state == 0 && now >= key_next {
                key_state = 1;
                key_next = now + Duration::from_millis(150);
                emulation
                    .consume(
                        Event::Keyboard(KeyboardEvent::Key {
                            time: 0,
                            key: 30,
                            state: 1,
                        }),
                        0,
                    )
                    .await?;
            } else if key_state == 1 && now >= key_next {
                key_state = 0;
                key_next = now + Duration::from_secs(2);
                emulation
                    .consume(
                        Event::Keyboard(KeyboardEvent::Key {
                            time: 0,
                            key: 30,
                            state: 0,
                        }),
                        0,
                    )
                    .await?;
            }
        }
    }
}
