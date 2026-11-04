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
            value: if initial.is_finite() { initial } else { 0.0 },
            velocity: 0.0,
            target: if initial.is_finite() { initial } else { 0.0 },
            config,
        }
    }

    pub fn set_target(&mut self, target: f32) {
        if target.is_finite() {
            self.target = target;
        }
    }

    /// Advance the exact damped oscillator solution. Unlike Euler integration,
    /// this remains stable after long frame stalls and does not depend on refresh rate.
    /// Invalid elapsed times/configuration do not advance the animation.
    pub fn step(&mut self, dt: f32) -> f32 {
        if !self.value.is_finite() {
            self.value = if self.target.is_finite() {
                self.target
            } else {
                0.0
            };
        }
        if !self.target.is_finite() {
            self.target = self.value;
        }
        if !self.velocity.is_finite() {
            self.velocity = 0.0;
        }
        if !dt.is_finite()
            || dt <= 0.0
            || !self.config.angular_frequency.is_finite()
            || self.config.angular_frequency <= 0.0
            || !self.config.damping_ratio.is_finite()
            || self.config.damping_ratio < 0.0
        {
            return self.value;
        }
        // Use f64 intermediates so finite f32 inputs cannot overflow the solution.
        let t = f64::from(dt);
        let omega = f64::from(self.config.angular_frequency);
        let zeta = f64::from(self.config.damping_ratio);
        let x = f64::from(self.value) - f64::from(self.target);
        let velocity = f64::from(self.velocity);
        let (displacement, next_velocity) = if (zeta - 1.0).abs() < 1e-6 {
            let decay = (-omega * t).exp();
            let coefficient = velocity + omega * x;
            (
                (x + coefficient * t) * decay,
                (velocity - omega * coefficient * t) * decay,
            )
        } else if zeta < 1.0 {
            let damped = omega * (1.0 - zeta * zeta).sqrt();
            let (sin, cos) = (damped * t).sin_cos();
            let decay = (-zeta * omega * t).exp();
            (
                decay * (x * cos + (velocity + zeta * omega * x) / damped * sin),
                decay
                    * (velocity * cos
                        - (zeta * omega * velocity + omega * omega * x) / damped * sin),
            )
        } else {
            let root = (zeta * zeta - 1.0).sqrt();
            let fast = -omega * (zeta + root);
            // Equivalent to -omega*(zeta-root), without catastrophic cancellation.
            let slow = -omega / (zeta + root);
            let a = (velocity - fast * x) / (slow - fast);
            let b = x - a;
            let a_decay = a * (slow * t).exp();
            let b_decay = b * (fast * t).exp();
            (a_decay + b_decay, slow * a_decay + fast * b_decay)
        };
        self.value = (f64::from(self.target) + displacement)
            .clamp(f64::from(f32::MIN), f64::from(f32::MAX)) as f32;
        self.velocity = next_velocity.clamp(f64::from(f32::MIN), f64::from(f32::MAX)) as f32;
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

    #[test]
    fn long_frame_stalls_remain_finite_and_converge() {
        for config in [
            SpringConfig::CRITICALLY_DAMPED,
            SpringConfig::FLUID_BOUNCE,
            SpringConfig {
                angular_frequency: 24.0,
                damping_ratio: 2.0,
            },
        ] {
            let mut spring = Spring1D::new(0.0, config);
            spring.set_target(100.0);
            let value = spring.step(1.0);
            assert!(
                value.is_finite() && (value - 100.0).abs() < 1.0,
                "{config:?}: {spring:?}"
            );
            spring.step(100.0);
            assert!(spring.is_settled());
            assert_eq!(spring.value, 100.0);
        }
    }

    #[test]
    fn analytic_solution_is_independent_of_frame_rate() {
        for config in [
            SpringConfig::CRITICALLY_DAMPED,
            SpringConfig::FLUID_BOUNCE,
            SpringConfig {
                angular_frequency: 24.0,
                damping_ratio: 2.0,
            },
        ] {
            let mut one_step = Spring1D::new(-100.0, config);
            one_step.set_target(100.0);
            let mut small_steps = one_step;
            one_step.step(0.2);
            for _ in 0..24 {
                small_steps.step(1.0 / 120.0);
            }
            assert!((one_step.value - small_steps.value).abs() < 0.001);
            assert!((one_step.velocity - small_steps.velocity).abs() < 0.01);
        }
    }

    #[test]
    fn invalid_time_target_and_configuration_do_not_poison_spring() {
        let mut spring = Spring1D::new(42.0, SpringConfig::CRITICALLY_DAMPED);
        for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            spring.set_target(invalid);
            assert_eq!(spring.target, 42.0);
            assert_eq!(spring.step(invalid), 42.0);
        }
        spring.config.angular_frequency = f32::NAN;
        assert_eq!(spring.step(0.1), 42.0);
        spring.value = f32::NAN;
        spring.target = f32::INFINITY;
        spring.velocity = f32::NAN;
        assert!(spring.step(0.1).is_finite());
        assert!(spring.velocity.is_finite() && spring.target.is_finite());
    }
}
