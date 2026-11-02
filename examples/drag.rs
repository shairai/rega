//! Dev helper for UI testing: `cargo run --example drag -- x0 y0 x1 y1`
//! drags the mouse between two global logical points.
use enigo::{Button, Coordinate, Direction, Enigo, Mouse, Settings};
use std::{thread::sleep, time::Duration};

fn main() {
    let v: Vec<i32> = std::env::args().skip(1).filter_map(|a| a.parse().ok()).collect();
    let [x0, y0, x1, y1] = v[..] else { panic!("usage: drag x0 y0 x1 y1") };
    let mut e = Enigo::new(&Settings::default()).unwrap();
    e.move_mouse(x0, y0, Coordinate::Abs).unwrap();
    sleep(Duration::from_millis(150));
    e.button(Button::Left, Direction::Press).unwrap();
    for i in 1..=20 {
        e.move_mouse(x0 + (x1 - x0) * i / 20, y0 + (y1 - y0) * i / 20, Coordinate::Abs).unwrap();
        sleep(Duration::from_millis(15));
    }
    e.button(Button::Left, Direction::Release).unwrap();
}
