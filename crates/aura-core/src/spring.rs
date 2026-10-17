//! Critically damped spring physics for interruptible macOS-style window and Dock animations.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SpringConfig {
    /// Natural undamped angular frequency $\omega_0$ in rad/s (higher = snappier).
    pub angular_frequency: f32,
    /// Damping ratio $\zeta$ (`1.0` = critically damped with zero overshoot, `0.86` = gentle bounce).
    pub damping_ratio: f32,
}

impl SpringConfig {
    /// Snappy, zero-overshoot spring for Dock magnification and window snapping.
    pub const CRITICALLY_DAMPED: Self = Self {
        angular_frequency: 24.0,
        damping_ratio: 1.0,
    };

    /// Fluid, slightly buoyant spring for Mission Control and Stage Manager transitions.
    pub const FLUID_BOUNCE: Self = Self {
        angular_frequency: 18.0,
        damping_ratio: 0.86,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Spring1D {
    pub value: f32,
    pub velocity: f32,
    pub target: f32,
    pub config: SpringConfig,
}

impl Spring1D {
    pub fn new(initial: f32, config: SpringConfig) -> Self {
        Self {
            value: initial,
            velocity: 0.0,
            target: initial,
            config,
        }
    }

    pub fn set_target(&mut self, target: f32) {
        self.target = target;
    }

    /// Advance the spring state by `dt` seconds using semi-implicit Euler integration
    /// sub-stepped for unconditional stability across variable refresh rates (60Hz–240Hz).
    pub fn step(&mut self, dt: f32) -> f32 {
        if dt <= 0.0 {
            return self.value;
        }
        let substeps = ((dt / (1.0 / 240.0)).ceil() as usize).clamp(1, 16);
        let h = dt / (substeps as f32);
        let omega = self.config.angular_frequency;
        let zeta = self.config.damping_ratio;

        for _ in 0..substeps {
            let displacement = self.value - self.target;
            let accel = -omega * omega * displacement - 2.0 * zeta * omega * self.velocity;
            self.velocity += accel * h;
            self.value += self.velocity * h;
        }

        if (self.value - self.target).abs() < 0.01 && self.velocity.abs() < 0.05 {
            self.value = self.target;
            self.velocity = 0.0;
        }
        self.value
    }

    pub fn is_settled(&self) -> bool {
        (self.value - self.target).abs() < 0.01 && self.velocity.abs() < 0.05
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SpringRect {
    pub x: Spring1D,
    pub y: Spring1D,
    pub width: Spring1D,
    pub height: Spring1D,
}

impl SpringRect {
    pub fn new(x: f32, y: f32, width: f32, height: f32, config: SpringConfig) -> Self {
        Self {
            x: Spring1D::new(x, config),
            y: Spring1D::new(y, config),
            width: Spring1D::new(width, config),
            height: Spring1D::new(height, config),
        }
    }

    pub fn set_target(&mut self, x: f32, y: f32, width: f32, height: f32) {
        self.x.set_target(x);
        self.y.set_target(y);
        self.width.set_target(width);
        self.height.set_target(height);
    }

    pub fn step(&mut self, dt: f32) -> (f32, f32, f32, f32) {
        (
            self.x.step(dt),
            self.y.step(dt),
            self.width.step(dt),
            self.height.step(dt),
        )
    }

    pub fn is_settled(&self) -> bool {
        self.x.is_settled()
            && self.y.is_settled()
            && self.width.is_settled()
            && self.height.is_settled()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn critically_damped_spring_converges_without_overshoot() {
        let mut s = Spring1D::new(0.0, SpringConfig::CRITICALLY_DAMPED);
        s.set_target(100.0);
        let mut max_seen = 0.0_f32;
        for _ in 0..120 {
            let v = s.step(1.0 / 120.0);
            max_seen = max_seen.max(v);
        }
        assert!(s.is_settled());
        assert_eq!(s.value, 100.0);
        assert!(max_seen <= 100.05, "unexpected overshoot: {max_seen}");
    }
}
