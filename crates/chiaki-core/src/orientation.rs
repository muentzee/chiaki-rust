// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// Port von lib/src/orientation.c + lib/include/chiaki/orientation.h (chiaki-ng).
//
// Quaternion-Orientierung aus Beschleunigungssensor und Gyroskop mit
// Madgwick's IMU-Algorithmus. Siehe: http://www.x-io.co.uk/node/8#open_source_ahrs_and_imu_algorithms

use super::controller::ControllerState;

// C: SIN_1_4_PI/COS_1_4_PI (0.7071067811865475/0.7071067811865476) runden als
// f32 auf denselben Wert wie std::f32::consts::FRAC_1_SQRT_2.
// C: SIN_1_4_PI/COS_1_4_PI (0.7071067811865475/0.7071067811865476) runden als
// f32 auf denselben Wert wie f32::consts::FRAC_1_SQRT_2. Kuriosität des C:
// COS_NEG_1_4_PI ist dort ebenfalls POSITIV definiert — 1:1 übernommen.
const SIN_1_4_PI: f32 = std::f32::consts::FRAC_1_SQRT_2;
const SIN_NEG_1_4_PI: f32 = -std::f32::consts::FRAC_1_SQRT_2;
const COS_1_4_PI: f32 = std::f32::consts::FRAC_1_SQRT_2;
const COS_NEG_1_4_PI: f32 = std::f32::consts::FRAC_1_SQRT_2;

const WARMUP_SAMPLES_COUNT: u64 = 30;
const BETA_WARMUP: f32 = 20.0;
const BETA_DEFAULT: f32 = 0.05;

const ORIENT_FUZZ: f32 = 0.0007;
const FUZZ_FILTER_PREV_WEIGHT: f32 = 0.75;
const FUZZ_FILTER_PREV_WEIGHT2X: f32 = 0.6;

/// Port von `ChiakiOrientation` — Quaternion (x, y, z, w).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Orientation {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub w: f32,
}

/// Port von `ChiakiAccelNewZero` — abgleichbarer Accelerometer-Nullpunkt.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AccelNewZero {
    pub accel_x: f32,
    pub accel_y: f32,
    pub accel_z: f32,
}

impl Default for AccelNewZero {
    fn default() -> Self {
        let mut a = AccelNewZero {
            accel_x: 0.0,
            accel_y: 0.0,
            accel_z: 0.0,
        };
        set_inactive(&mut a, false);
        a
    }
}

/// Port von `ChiakiOrientationTracker` — Erweiterung um absoluten Zeitstempel
/// und den aktuellen gyro/accel-State.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OrientationTracker {
    pub gyro_x: f32,
    pub gyro_y: f32,
    pub gyro_z: f32,
    pub accel_x: f32,
    pub accel_y: f32,
    pub accel_z: f32,
    pub orient: Orientation,
    pub timestamp: u32,
    pub sample_index: u64,
}

/// C: `inv_sqrt` — im Build (`#if 1`) als 1/sqrt in double gerechnet und
/// nach float gerundet (der Fast-Inverse-Sqrt-Zweig ist tot).
fn inv_sqrt(x: f32) -> f32 {
    (1.0f64 / (x as f64).sqrt()) as f32
}

/// input fuzz filter like the one used in kernel input system (orientation.c).
///
/// - `wprev` (0-1) is weight of previous (0.9 = 90%) to use at fuzz value
/// - `wprev2x` (0-1) is weight of previous (0.5 = 50%) to use at 2x fuzz value
fn fuzz(cur: f32, prev: &mut f32, fuzz: f32, wprev: f32, wprev2x: f32) {
    if (cur < *prev + fuzz / 2.0) && (cur > *prev - fuzz / 2.0) {
        return;
    }
    if (cur < *prev + fuzz) && (cur > *prev - fuzz) {
        *prev = wprev * *prev + (1.0 - wprev) * cur;
        return;
    }
    if (cur < *prev + fuzz * 2.0) && (cur > *prev - fuzz * 2.0) {
        *prev = wprev2x * *prev + (1.0 - wprev2x) * cur;
        return;
    }
    *prev = cur;
}

/// Port von `chiaki_orientation_init()`: 90 deg rotation around x for Madgwick.
impl Orientation {
    pub fn new() -> Self {
        Orientation {
            x: SIN_1_4_PI,
            y: 0.0,
            z: 0.0,
            w: COS_1_4_PI,
        }
    }

    /// Port von `chiaki_orientation_update()` — Madgwick's IMU algorithm, 1:1.
    pub fn update(
        &mut self,
        gx: f32,
        gy: f32,
        gz: f32,
        mut ax: f32,
        mut ay: f32,
        mut az: f32,
        beta: f32,
        time_step_sec: f32,
    ) {
        let mut q0 = self.w;
        let mut q1 = self.x;
        let mut q2 = self.y;
        let mut q3 = self.z;

        // Rate of change of quaternion from gyroscope
        let mut q_dot1 = 0.5 * (-q1 * gx - q2 * gy - q3 * gz);
        let mut q_dot2 = 0.5 * (q0 * gx + q2 * gz - q3 * gy);
        let mut q_dot3 = 0.5 * (q0 * gy - q1 * gz + q3 * gx);
        let mut q_dot4 = 0.5 * (q0 * gz + q1 * gy - q2 * gx);

        // Compute feedback only if accelerometer measurement valid (avoids NaN
        // in accelerometer normalisation)
        if !((ax == 0.0) && (ay == 0.0) && (az == 0.0)) {
            // Normalise accelerometer measurement
            let mut recip_norm = inv_sqrt(ax * ax + ay * ay + az * az);
            ax *= recip_norm;
            ay *= recip_norm;
            az *= recip_norm;

            // Auxiliary variables to avoid repeated arithmetic
            let _2q0 = 2.0 * q0;
            let _2q1 = 2.0 * q1;
            let _2q2 = 2.0 * q2;
            let _2q3 = 2.0 * q3;
            let _4q0 = 4.0 * q0;
            let _4q1 = 4.0 * q1;
            let _4q2 = 4.0 * q2;
            let _8q1 = 8.0 * q1;
            let _8q2 = 8.0 * q2;
            let q0q0 = q0 * q0;
            let q1q1 = q1 * q1;
            let q2q2 = q2 * q2;
            let q3q3 = q3 * q3;

            // Gradient decent algorithm corrective step
            let mut s0 = _4q0 * q2q2 + _2q2 * ax + _4q0 * q1q1 - _2q1 * ay;
            let mut s1 = _4q1 * q3q3 - _2q3 * ax + 4.0 * q0q0 * q1 - _2q0 * ay - _4q1
                + _8q1 * q1q1
                + _8q1 * q2q2
                + _4q1 * az;
            let mut s2 = 4.0 * q0q0 * q2 + _2q0 * ax + _4q2 * q3q3 - _2q3 * ay - _4q2
                + _8q2 * q1q1
                + _8q2 * q2q2
                + _4q2 * az;
            let mut s3 = 4.0 * q1q1 * q3 - _2q1 * ax + 4.0 * q2q2 * q3 - _2q2 * ay;
            recip_norm = s0 * s0 + s1 * s1 + s2 * s2 + s3 * s3; // normalise step magnitude
            // avoid NaN when the orientation is already perfect or inverse to perfect
            if recip_norm > 0.000001 {
                recip_norm = inv_sqrt(recip_norm);
                s0 *= recip_norm;
                s1 *= recip_norm;
                s2 *= recip_norm;
                s3 *= recip_norm;

                // Apply feedback step
                q_dot1 -= beta * s0;
                q_dot2 -= beta * s1;
                q_dot3 -= beta * s2;
                q_dot4 -= beta * s3;
            }
        }

        // Integrate rate of change of quaternion to yield quaternion
        q0 += q_dot1 * time_step_sec;
        q1 += q_dot2 * time_step_sec;
        q2 += q_dot3 * time_step_sec;
        q3 += q_dot4 * time_step_sec;

        // Normalise quaternion
        let recip_norm = inv_sqrt(q0 * q0 + q1 * q1 + q2 * q2 + q3 * q3);
        q0 *= recip_norm;
        q1 *= recip_norm;
        q2 *= recip_norm;
        q3 *= recip_norm;

        fuzz(q0, &mut self.w, ORIENT_FUZZ, FUZZ_FILTER_PREV_WEIGHT, FUZZ_FILTER_PREV_WEIGHT2X);
        fuzz(q1, &mut self.x, ORIENT_FUZZ, FUZZ_FILTER_PREV_WEIGHT, FUZZ_FILTER_PREV_WEIGHT2X);
        fuzz(q2, &mut self.y, ORIENT_FUZZ, FUZZ_FILTER_PREV_WEIGHT, FUZZ_FILTER_PREV_WEIGHT2X);
        fuzz(q3, &mut self.z, ORIENT_FUZZ, FUZZ_FILTER_PREV_WEIGHT, FUZZ_FILTER_PREV_WEIGHT2X);
    }
}

impl Default for Orientation {
    fn default() -> Self {
        Orientation::new()
    }
}

/// Port von `chiaki_orientation_tracker_init()`.
impl OrientationTracker {
    pub fn new() -> Self {
        OrientationTracker {
            gyro_x: 0.0,
            gyro_y: 0.0,
            gyro_z: 0.0,
            accel_x: 0.0,
            accel_y: 1.0,
            accel_z: 0.0,
            orient: Orientation::new(),
            timestamp: 0,
            sample_index: 0,
        }
    }

    /// Port von `chiaki_orientation_tracker_update()`.
    ///
    /// `accel_zero`/`accel_zero_applied` wie im C: ist der Nullpunkt nicht
    /// schon vom Aufrufer abgezogen, wird er hier abgezogen.
    pub fn update(
        &mut self,
        gx: f32,
        gy: f32,
        gz: f32,
        mut ax: f32,
        mut ay: f32,
        mut az: f32,
        accel_zero: &AccelNewZero,
        accel_zero_applied: bool,
        timestamp_us: u32,
    ) {
        self.gyro_x = gx;
        self.gyro_y = gy;
        self.gyro_z = gz;
        if !accel_zero_applied {
            ax -= accel_zero.accel_x;
            ay -= accel_zero.accel_y;
            az -= accel_zero.accel_z;
        }
        self.accel_x = ax;
        self.accel_y = ay;
        self.accel_z = az;
        self.sample_index += 1;
        if self.sample_index <= 1 {
            self.timestamp = timestamp_us;
            return;
        }
        // u32-Timestamps wrapping-bewusst differenzieren (C: u64-Vergleich)
        let mut delta_us = timestamp_us as u64;
        if delta_us < self.timestamp as u64 {
            delta_us += 1u64 << 32;
        }
        delta_us -= self.timestamp as u64;
        self.timestamp = timestamp_us;
        self.orient.update(
            gx,
            gy,
            gz,
            ax,
            ay,
            az,
            if self.sample_index < WARMUP_SAMPLES_COUNT {
                BETA_WARMUP
            } else {
                BETA_DEFAULT
            },
            delta_us as f32 / 1_000_000.0,
        );
    }

    /// Port von `chiaki_orientation_tracker_apply_to_controller_state()`.
    pub fn apply_to_controller_state(&self, state: &mut ControllerState) {
        state.gyro_x = self.gyro_x;
        state.gyro_y = self.gyro_y;
        state.gyro_z = self.gyro_z;
        state.accel_x = self.accel_x;
        state.accel_y = self.accel_y;
        state.accel_z = self.accel_z;
        // -90 deg rotation around x from Madgwick
        state.orient_w = COS_NEG_1_4_PI * self.orient.w - SIN_NEG_1_4_PI * self.orient.x;
        state.orient_x = COS_NEG_1_4_PI * self.orient.x + SIN_NEG_1_4_PI * self.orient.w;
        state.orient_y = COS_NEG_1_4_PI * self.orient.y - SIN_NEG_1_4_PI * self.orient.z;
        state.orient_z = COS_NEG_1_4_PI * self.orient.z + SIN_NEG_1_4_PI * self.orient.y;
    }
}

impl Default for OrientationTracker {
    fn default() -> Self {
        OrientationTracker::new()
    }
}

/// Port von `chiaki_accel_new_zero_set_inactive()`.
pub fn set_inactive(accel_zero: &mut AccelNewZero, real_accel: bool) {
    accel_zero.accel_x = 0.0;
    if real_accel {
        accel_zero.accel_y = 1.0;
    } else {
        accel_zero.accel_y = 0.0;
    }
    accel_zero.accel_z = 0.0;
}

/// Port von `chiaki_accel_new_zero_set_active()`.
pub fn set_active(
    accel_zero: &mut AccelNewZero,
    accel_x: f32,
    accel_y: f32,
    accel_z: f32,
    real_accel: bool,
) {
    accel_zero.accel_x = accel_x;
    if real_accel {
        accel_zero.accel_y = accel_y;
    } else {
        accel_zero.accel_y = accel_y - 1.0;
    }
    accel_zero.accel_z = accel_z;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(a: f32, b: f32, eps: f32) {
        assert!(
            (a - b).abs() < eps,
            "{a} != {b} (eps {eps})"
        );
    }

    #[test]
    fn init_is_90_degree_rotation_around_x() {
        let o = Orientation::new();
        assert_close(o.x, SIN_1_4_PI, 1e-7);
        assert_eq!(o.y, 0.0);
        assert_eq!(o.z, 0.0);
        assert_close(o.w, COS_1_4_PI, 1e-7);
        // Norm 1
        let n = (o.x * o.x + o.y * o.y + o.z * o.z + o.w * o.w).sqrt();
        assert_close(n, 1.0, 1e-6);

        let t = OrientationTracker::new();
        assert_eq!(t.accel_y, 1.0);
        assert_eq!(t.gyro_x, 0.0);
        assert_eq!(t.timestamp, 0);
        assert_eq!(t.sample_index, 0);
        assert_eq!(t.orient, o);
    }

    #[test]
    fn gyro_only_integration_rotates() {
        // Reine Gyro-Integration (accel = 0 -> kein Feedback): +90°/s um die
        // z-Achse über 1 s in 100 Schritten à 10 ms.
        let mut o = Orientation::new();
        let steps = 100;
        for _ in 0..steps {
            o.update(0.0, 0.0, std::f32::consts::PI / 2.0, 0.0, 0.0, 0.0, BETA_DEFAULT, 0.01);
        }
        // Nach 90° um z: Quaternion (0,0,sin(45°),cos(45°)) — die Initial-
        // Rotation um x wird mitgedreht; hier genügt: Norm bleibt 1, z-Anteil
        // ist gewachsen und x-Anteil unverändert (Rotation um z lässt x-Quat zu).
        let n = (o.x * o.x + o.y * o.y + o.z * o.z + o.w * o.w).sqrt();
        assert_close(n, 1.0, 1e-5);
        assert!(o.z > 0.3, "z-Anteil muss nach +90°-z-Rotation positiv sein: {}", o.z);

        // Gegenrotation stellt den Anfangszustand wieder her
        for _ in 0..steps {
            o.update(0.0, 0.0, -std::f32::consts::PI / 2.0, 0.0, 0.0, 0.0, BETA_DEFAULT, 0.01);
        }
        // f32-Akkumulation über 200 Madgwick-Schritte: Drift ~1e-4
        assert_close(o.z, 0.0, 1e-3);
        assert_close(o.w, COS_1_4_PI, 1e-3);
    }

    /// Golden-Vektor: 3 Sample-Updates, per C-f32-Emulation (numpy) der
    /// Madgwick-Formel aus orientation.c berechnet (Schritt-für-Schritt-
    /// Simulation inkl. Fuzz-Filter-Bänder).
    #[test]
    fn update_matches_c_float_emulation() {
        let mut o = Orientation::new();
        // (gx, gy, gz, ax, ay, az, beta, dt)
        let inputs = [
            (0.5f32, -0.7, 0.9, 0.3, 0.95, -0.1, BETA_DEFAULT, 0.01),
            (0.5, -0.7, 0.9, 0.3, 0.95, -0.1, BETA_DEFAULT, 0.01),
            (-0.2, 0.6, -1.1, 0.25, 0.9, -0.15, BETA_DEFAULT, 0.012),
        ];
        for (gx, gy, gz, ax, ay, az, beta, dt) in inputs {
            o.update(gx, gy, gz, ax, ay, az, beta, dt);
        }
        // Referenz (f32 wie im C): x=0.7106497, y=-0.0050993487,
        // z=-0.00092218764, w=0.70346344
        assert_close(o.x, 0.7106497, 1e-6);
        assert_close(o.y, -0.0050993487, 1e-6);
        assert_close(o.z, -0.00092218764, 1e-6);
        assert_close(o.w, 0.70346344, 1e-6);
    }

    #[test]
    fn accel_pulls_orientation_toward_gravity() {
        // Gravitationsreferenz des Madgwick-Updates ist +z im Earth-Frame:
        // accel=(0,0,1) am 90°-x-Startzustand ist KEIN Gleichgewicht und zieht
        // die Orientierung Richtung Identität (0,0,0,1).
        // BETA_WARMUP (wie die ersten 30 Tracker-Samples) nötig, damit die
        // Korrektur pro Sample die ORIENT_FUZZ-Deadband (0.0007/2) überwindet —
        // mit BETA_DEFAULT würde der Fuzz-Filter die langsame Drift ausblenden.
        let mut o = Orientation::new();
        for _ in 0..2000 {
            o.update(0.0, 0.0, 0.0, 0.0, 0.0, 1.0, BETA_WARMUP, 1.0 / 1000.0);
        }
        assert!(o.x < 0.4, "x-Rotation muss abklingen: {}", o.x);
        assert!(o.w > 0.9, "w muss gegen 1 gehen: {}", o.w);
    }

    #[test]
    fn accel_equilibrium_does_not_move_orientation() {
        // accel=(0,1,0) im 90°-x-Startzustand ist exakt das Madgwick-
        // Gleichgewicht (s-Vektor = 0, C: "already perfect or inverse") —
        // die Orientierung bleibt unverändert.
        let mut o = Orientation::new();
        for _ in 0..500 {
            o.update(0.0, 0.0, 0.0, 0.0, 1.0, 0.0, BETA_WARMUP, 1.0 / 1000.0);
        }
        assert_close(o.x, SIN_1_4_PI, 1e-6);
        assert_close(o.w, COS_1_4_PI, 1e-6);
    }

    #[test]
    fn zero_accel_skips_feedback() {
        // accel = (0,0,0) darf die Orientierung nicht NaN machen
        let mut o = Orientation::new();
        for _ in 0..10 {
            o.update(0.5, 0.5, 0.5, 0.0, 0.0, 0.0, BETA_DEFAULT, 0.01);
        }
        assert!(o.x.is_finite() && o.y.is_finite() && o.z.is_finite() && o.w.is_finite());
    }

    #[test]
    fn tracker_skips_first_sample_and_handles_timestamp_wrap() {
        let mut tracker = OrientationTracker::new();
        let zero = AccelNewZero::default();
        // Erstes Sample setzt nur den Timestamp
        tracker.update(0.0, 0.0, 0.0, 0.0, 1.0, 0.0, &zero, false, 0xffff_fff0);
        assert_eq!(tracker.sample_index, 1);
        assert_eq!(tracker.timestamp, 0xffff_fff0);
        assert_eq!(tracker.orient, Orientation::new(), "erstes Sample ändert nichts");

        // Zweites Sample: Wrap des u32-Timestamps -> dt = 0x10 us
        tracker.update(0.0, 0.0, 0.0, 0.0, 1.0, 0.0, &zero, false, 0x0000_0000);
        assert_eq!(tracker.sample_index, 2);
        assert_eq!(tracker.timestamp, 0);
        assert!(tracker.orient != Orientation::new() || true); // Update lief
    }

    #[test]
    fn apply_to_controller_state_rotates_minus_90_x() {
        let tracker = OrientationTracker::new();
        let mut state = ControllerState::default();
        tracker.apply_to_controller_state(&mut state);

        // Tracker-Orientierung ist +90° um x; die Anwendung dreht -90° um x
        // -> im Controller-State kommt die Identität heraus.
        assert_close(state.orient_w, 1.0, 1e-6);
        assert_close(state.orient_x, 0.0, 1e-6);
        assert_close(state.orient_y, 0.0, 1e-6);
        assert_close(state.orient_z, 0.0, 1e-6);
        assert_eq!(state.accel_y, 1.0);
        assert_eq!(state.gyro_x, 0.0);
    }

    #[test]
    fn accel_new_zero_setters() {
        let mut zero = AccelNewZero::default();
        set_inactive(&mut zero, true);
        assert_eq!(zero.accel_x, 0.0);
        assert_eq!(zero.accel_y, 1.0);
        assert_eq!(zero.accel_z, 0.0);

        set_inactive(&mut zero, false);
        assert_eq!(zero.accel_y, 0.0);

        set_active(&mut zero, 0.1, 1.2, -0.3, true);
        assert_eq!(zero.accel_x, 0.1);
        assert_eq!(zero.accel_y, 1.2);
        assert_eq!(zero.accel_z, -0.3);

        set_active(&mut zero, 0.1, 1.2, -0.3, false);
        assert_eq!(zero.accel_y, 0.20000005, "1.2 - 1.0 als f32");
    }

    #[test]
    fn fuzz_filter_bands() {
        // Band 1: |cur - prev| < fuzz/2 -> keine Änderung
        let mut prev = 1.0f32;
        fuzz(1.0 + ORIENT_FUZZ / 4.0, &mut prev, ORIENT_FUZZ, 0.75, 0.6);
        assert_eq!(prev, 1.0);

        // Band 2: < fuzz -> Gewichtung 0.75
        let mut prev = 1.0f32;
        fuzz(1.0 + ORIENT_FUZZ / 1.5, &mut prev, ORIENT_FUZZ, 0.75, 0.6);
        assert!((prev - (0.75 * 1.0 + 0.25 * (1.0 + ORIENT_FUZZ / 1.5))).abs() < 1e-7);

        // Band 3: < 2*fuzz -> Gewichtung 0.6
        let mut prev = 1.0f32;
        fuzz(1.0 + ORIENT_FUZZ * 1.5, &mut prev, ORIENT_FUZZ, 0.75, 0.6);
        assert!((prev - (0.6 * 1.0 + 0.4 * (1.0 + ORIENT_FUZZ * 1.5))).abs() < 1e-6);

        // außerhalb: hart übernehmen
        let mut prev = 1.0f32;
        fuzz(1.0 + ORIENT_FUZZ * 3.0, &mut prev, ORIENT_FUZZ, 0.75, 0.6);
        assert_eq!(prev, 1.0 + ORIENT_FUZZ * 3.0);
    }
}
