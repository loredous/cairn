//! Small numerical helpers with the semantics of the NumPy/SciPy calls the
//! Stage 1 receiver uses (`savgol_filter(mode="interp")`, `np.convolve(mode="same")`,
//! `np.interp`, `np.unwrap`, `np.median`, `firwin` with a Kaiser window).

use std::f64::consts::PI;

use num_complex::Complex64;

/// Least-squares weights: value at position `at` of the degree-`order`
/// polynomial fitted to `len` equally spaced points.
fn poly_fit_weights(len: usize, order: usize, at: f64) -> Vec<f64> {
    let c = (len as f64 - 1.0) / 2.0;
    let p = order + 1;
    // Normal matrix V^T V with centered abscissae.
    let mut a = vec![vec![0.0; p]; p];
    for t in 0..len {
        let u = t as f64 - c;
        for (i, row) in a.iter_mut().enumerate() {
            for (j, v) in row.iter_mut().enumerate() {
                *v += u.powi((i + j) as i32);
            }
        }
    }
    // Solve (V^T V) b = e(at), with e(at) = [1, u, u^2, ...] at the evaluation point.
    let ua = at - c;
    let mut rhs: Vec<f64> = (0..p).map(|i| ua.powi(i as i32)).collect();
    for col in 0..p {
        let piv = (col..p).max_by(|&x, &y| a[x][col].abs().total_cmp(&a[y][col].abs())).unwrap();
        a.swap(col, piv);
        rhs.swap(col, piv);
        for r in 0..p {
            if r != col {
                let f = a[r][col] / a[col][col];
                if f != 0.0 {
                    let pivot_row = a[col].clone();
                    for (x, y) in a[r][col..].iter_mut().zip(&pivot_row[col..]) {
                        *x -= f * y;
                    }
                    rhs[r] -= f * rhs[col];
                }
            }
        }
    }
    let b: Vec<f64> = (0..p).map(|i| rhs[i] / a[i][i]).collect();
    (0..len)
        .map(|t| {
            let u = t as f64 - c;
            (0..p).map(|i| b[i] * u.powi(i as i32)).sum()
        })
        .collect()
}

/// `scipy.signal.savgol_filter(x, window, order, mode="interp")`.
pub fn savgol_interp(x: &[f64], window: usize, order: usize) -> Vec<f64> {
    let n = x.len();
    assert!(window % 2 == 1 && window <= n && order < window);
    let half = window / 2;
    let centre = poly_fit_weights(window, order, half as f64);
    let mut out = vec![0.0; n];
    for i in half..n - half {
        out[i] = centre.iter().zip(&x[i - half..i + half + 1]).map(|(w, v)| w * v).sum();
    }
    for i in 0..half {
        let w = poly_fit_weights(window, order, i as f64);
        out[i] = w.iter().zip(&x[..window]).map(|(w, v)| w * v).sum();
        let j = n - half + i;
        let w = poly_fit_weights(window, order, (window - half + i) as f64);
        out[j] = w.iter().zip(&x[n - window..]).map(|(w, v)| w * v).sum();
    }
    out
}

/// `np.convolve(a, v, mode="same")` for real sequences.
pub fn convolve_same(a: &[f64], v: &[f64]) -> Vec<f64> {
    let (m, n) = (a.len(), v.len());
    let start = (m.min(n) - 1) / 2;
    (0..m.max(n))
        .map(|o| {
            let i = o + start; // index into the full convolution
            let lo = i.saturating_sub(n - 1);
            let hi = i.min(m - 1);
            (lo..=hi).map(|j| a[j] * v[i - j]).sum()
        })
        .collect()
}

/// `np.convolve(a, v, mode="same")` for a complex sequence and a real kernel.
pub fn convolve_same_complex(a: &[Complex64], v: &[f64]) -> Vec<Complex64> {
    let (m, n) = (a.len(), v.len());
    let start = (m.min(n) - 1) / 2;
    (0..m.max(n))
        .map(|o| {
            let i = o + start;
            let lo = i.saturating_sub(n - 1);
            let hi = i.min(m - 1);
            (lo..=hi).map(|j| a[j] * v[i - j]).sum()
        })
        .collect()
}

/// Box-average of `x` over `window` tones with `np.convolve(mode="same")`,
/// divided by the number of points actually summed.
pub fn box_average(x: &[f64], window: usize) -> Vec<f64> {
    let k = vec![1.0 / window as f64; window];
    let num = convolve_same(x, &k);
    let den = convolve_same(&vec![1.0; x.len()], &k);
    num.iter().zip(&den).map(|(a, b)| a / b).collect()
}

/// `np.interp(xq, xp, fp)` (xp increasing; constant beyond the ends).
pub fn interp(xq: f64, xp: &[f64], fp: &[f64]) -> f64 {
    let n = xp.len();
    if xq <= xp[0] {
        return fp[0];
    }
    if xq >= xp[n - 1] {
        return fp[n - 1];
    }
    let j = xp.partition_point(|&v| v <= xq) - 1;
    let t = (xq - xp[j]) / (xp[j + 1] - xp[j]);
    fp[j] + t * (fp[j + 1] - fp[j])
}

/// `np.unwrap` for a phase sequence.
pub fn unwrap(p: &[f64]) -> Vec<f64> {
    let mut out = Vec::with_capacity(p.len());
    let mut acc = 0.0;
    for (i, &v) in p.iter().enumerate() {
        if i > 0 {
            let dd = v - p[i - 1];
            let mut ddmod = (dd + PI).rem_euclid(2.0 * PI) - PI;
            if ddmod == -PI && dd > 0.0 {
                ddmod = PI;
            }
            if dd.abs() >= PI {
                acc += ddmod - dd;
            }
        }
        out.push(v + acc);
    }
    out
}

/// `np.median`.
pub fn median(x: &[f64]) -> f64 {
    let mut v = x.to_vec();
    v.sort_by(f64::total_cmp);
    let n = v.len();
    if n % 2 == 1 { v[n / 2] } else { 0.5 * (v[n / 2 - 1] + v[n / 2]) }
}

/// Mean of a slice.
pub fn mean(x: &[f64]) -> f64 {
    x.iter().sum::<f64>() / x.len() as f64
}

/// Least-squares line y = slope x + intercept (`np.polyfit(x, y, 1)`).
pub fn linear_fit(x: &[f64], y: &[f64]) -> (f64, f64) {
    let mx = mean(x);
    let my = mean(y);
    let sxy: f64 = x.iter().zip(y).map(|(a, b)| (a - mx) * (b - my)).sum();
    let sxx: f64 = x.iter().map(|a| (a - mx) * (a - mx)).sum();
    let slope = sxy / sxx;
    (slope, my - slope * mx)
}

/// Modified Bessel function of the first kind, order 0.
pub fn bessel_i0(x: f64) -> f64 {
    let q = x * x / 4.0;
    let (mut sum, mut term, mut k) = (1.0, 1.0, 1.0);
    loop {
        term *= q / (k * k);
        sum += term;
        if term < sum * 1e-17 {
            return sum;
        }
        k += 1.0;
    }
}

/// `scipy.signal.firwin(numtaps, cutoff_hz, window=("kaiser", beta), fs)`: a
/// windowed-sinc low-pass scaled to unit gain at DC.
pub fn firwin_kaiser(numtaps: usize, cutoff_hz: f64, beta: f64, fs: f64) -> Vec<f64> {
    let c = cutoff_hz / (fs / 2.0);
    let alpha = (numtaps as f64 - 1.0) / 2.0;
    let i0b = bessel_i0(beta);
    let mut h: Vec<f64> = (0..numtaps)
        .map(|n| {
            let m = n as f64 - alpha;
            let x = c * m;
            let sinc = if x == 0.0 { 1.0 } else { (PI * x).sin() / (PI * x) };
            let r = (n as f64 - alpha) / alpha;
            let w = bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / i0b;
            c * sinc * w
        })
        .collect();
    let s: f64 = h.iter().sum();
    for v in h.iter_mut() {
        *v /= s;
    }
    h
}

/// Values more than `k_sigma` robust standard deviations above the median.
pub fn outliers(m: &[f64], k_sigma: f64) -> Vec<bool> {
    let med = median(m);
    let dev: Vec<f64> = m.iter().map(|v| (v - med).abs()).collect();
    let spread = 1.4826 * median(&dev) + 1e-9;
    m.iter().map(|v| v - med > k_sigma * spread).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convolve_same_matches_numpy() {
        let a: Vec<f64> = (0..5).map(|v| v as f64).collect();
        assert_eq!(convolve_same(&a, &[1.0; 3]), vec![1.0, 3.0, 6.0, 9.0, 7.0]);
        let a: Vec<f64> = (0..6).map(|v| v as f64).collect();
        assert_eq!(convolve_same(&a, &[1.0; 4]), vec![1.0, 3.0, 6.0, 10.0, 14.0, 12.0]);
        let a: Vec<f64> = (0..3).map(|v| v as f64).collect();
        assert_eq!(convolve_same(&a, &[1.0; 5]), vec![1.0, 3.0, 3.0, 3.0, 3.0]);
    }

    #[test]
    fn savgol_reproduces_polynomials() {
        let x: Vec<f64> = (0..24).map(|t| 1.0 + 0.5 * t as f64 - 0.03 * (t * t) as f64).collect();
        for (a, b) in savgol_interp(&x, 7, 2).iter().zip(&x) {
            assert!((a - b).abs() < 1e-10);
        }
        // scipy.signal.savgol_filter(np.arange(10.) ** 2 % 7, 5, 1, mode="interp")
        let y: Vec<f64> = (0..10).map(|t| ((t * t) % 7) as f64).collect();
        let want = [0.8, 1.3, 1.8, 2.6, 2.6, 1.8, 1.6, 2.0, 2.0, 2.0];
        for (a, b) in savgol_interp(&y, 5, 1).iter().zip(want) {
            assert!((a - b).abs() < 1e-12, "{a} {b}");
        }
    }

    #[test]
    fn unwrap_matches_numpy() {
        let p = [0.0, 3.0, -3.0, 2.9, -2.9, 0.1];
        let want = [0.0, 3.0, 3.28318531, 2.9, 3.38318531, 6.38318531];
        for (a, b) in unwrap(&p).iter().zip(want) {
            assert!((a - b).abs() < 1e-8);
        }
    }

    #[test]
    fn firwin_matches_scipy() {
        // scipy.signal.firwin(11, 1290, window=("kaiser", 6.8), fs=48000)
        let want = [0.00136066, 0.01723058, 0.05952962, 0.12536044, 0.1888342, 0.21536899];
        let h = firwin_kaiser(11, 1290.0, 6.8, 48000.0);
        for (a, b) in h.iter().zip(want) {
            assert!((a - b).abs() < 1e-8);
        }
    }
}
