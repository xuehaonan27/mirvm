#!/usr/bin/env mirvm
---
[dependencies]
statrs = { version = "=0.18.0", default-features = false }
---
// statrs 0.18 (statistics-heavy float) differential: descriptive statistics + 9 distribution
// families with pdf/pmf/cdf/sf/inverse_cdf bit-pattern anchors + parameter error paths +
// hypothesis-test p-values + special functions. default-features=false drops the default
// nalgebra/rand: rand sampling is nondeterministic and nalgebra serves only multivariate
// distributions (not covered here); none of it is needed. statrs is pinned to =0.18.0.
//
// Coverage:
// ① descriptive statistics: the Statistics trait (min/max/abs_min/abs_max/mean/variance/
//    std_dev/population_*/geometric_mean/harmonic_mean/quadratic_mean/covariance) +
//    Data/OrderStatistics (median/quartile/percentile/quantile/order_statistic/ranks, four
//    tie-break strategies) + out-of-range -> f64::NAN boundaries. Note: statrs 0.18 removed
//    data-level skewness/kurtosis (0.16's Data::skewness is gone, only distribution-level
//    remains), so sample central moment g1 and excess kurtosis are computed by hand here.
// ② distribution families: Normal/Gamma/Beta/StudentsT/ChiSquared/Exp/Uniform (continuous)
//    + Binomial/Poisson (discrete, u64 support), fixed points for pdf/ln_pdf/pmf/cdf/sf/
//    inverse_cdf all as to_bits; distribution moments (mean/variance/std_dev/entropy/
//    skewness Option) + the Mode/Median/Min/Max traits.
// ③ parameter error paths: at least one per distribution (negative sigma/zero rate/out-of-range
//    probability/NaN parameter/max<min, etc.), printing the static Display string.
// ④ tests: z-test, t-test (hand-computed statistic + distribution sf for a two-sided p),
//    chi-squared goodness of fit (Poisson expectation + ChiSquared sf), and the crate's own
//    fisher exact test with three alternatives + odds ratio + zero-row boundary.
// ⑤ special functions: the gamma/beta/erf/factorial families + the checked_beta error path.
//
// Determinism: fixed data and evaluation points; no randomness/time/threads/hash order;
// every float is pinned by to_bits.
use statrs::distribution::{
    Beta, Binomial, ChiSquared, Continuous, ContinuousCDF, Discrete, DiscreteCDF, Exp, Gamma,
    Normal, Poisson, StudentsT, Uniform,
};
use statrs::function::{beta, erf, factorial, gamma};
use statrs::statistics::{
    Data, Distribution, Max, Median, Min, Mode, OrderStatistics, RankTieBreaker, Statistics,
};
use statrs::stats_tests::{self, Alternative};

/// f64 -> bit-pattern hex (a deterministic anchor).
fn b(x: f64) -> String {
    format!("{:016x}", x.to_bits())
}

/// Option<f64> -> bit-pattern hex or None.
fn ob(x: Option<f64>) -> String {
    match x {
        Some(v) => b(v),
        None => "None".to_string(),
    }
}

/// Option<u64> -> decimal or None (the discrete distribution mode).
fn ou(x: Option<u64>) -> String {
    match x {
        Some(v) => v.to_string(),
        None => "None".to_string(),
    }
}

/// The five distribution moments (mean/variance/std_dev/entropy/skewness).
fn moments<D: Distribution<f64>>(label: &str, d: &D) {
    println!(
        "{label} moments mean={} var={} std={} entropy={} skew={}",
        ob(d.mean()),
        ob(d.variance()),
        ob(d.std_dev()),
        ob(d.entropy()),
        ob(d.skewness())
    );
}

/// Continuous distribution fixed-point evaluation: pdf/ln_pdf/cdf/sf over xs, inverse_cdf over ps.
fn probe_cont<D>(label: &str, d: &D, xs: &[f64], ps: &[f64])
where
    D: Continuous<f64, f64> + ContinuousCDF<f64, f64>,
{
    for &x in xs {
        println!(
            "{label} x={x} pdf={} ln_pdf={} cdf={} sf={}",
            b(d.pdf(x)),
            b(d.ln_pdf(x)),
            b(d.cdf(x)),
            b(d.sf(x))
        );
    }
    for &p in ps {
        println!("{label} p={p} inverse_cdf={}", b(d.inverse_cdf(p)));
    }
}

/// Discrete distribution fixed-point evaluation (u64 support): pmf/ln_pmf/cdf/sf over ks, inverse_cdf over ps.
fn probe_disc<D>(label: &str, d: &D, ks: &[u64], ps: &[f64])
where
    D: Discrete<u64, f64> + DiscreteCDF<u64, f64>,
{
    for &k in ks {
        println!(
            "{label} k={k} pmf={} ln_pmf={} cdf={} sf={}",
            b(d.pmf(k)),
            b(d.ln_pmf(k)),
            b(d.cdf(k)),
            b(d.sf(k))
        );
    }
    for &p in ps {
        println!("{label} p={p} inverse_cdf={}", d.inverse_cdf(p));
    }
}

fn main() {
    // ===== ① descriptive statistics =====
    let d1: [f64; 16] = [
        4.5, -2.25, 7.0, 0.5, 3.75, -1.0, 9.5, 2.25, 5.0, -3.5, 6.25, 1.5, 8.0, -0.75, 4.0, 2.75,
    ];
    let d2: [f64; 16] = [
        1.0, 2.5, -0.5, 3.25, 0.75, -1.5, 4.5, 2.0, 6.0, -2.0, 3.5, 0.25, 5.5, 1.25, -0.25, 7.5,
    ];
    let pos: [f64; 8] = [1.5, 2.0, 3.5, 4.0, 5.25, 6.75, 8.5, 9.0];

    println!(
        "desc n={} min={} max={} abs_min={} abs_max={}",
        d1.len(),
        b(d1.min()),
        b(d1.max()),
        b(d1.abs_min()),
        b(d1.abs_max())
    );
    println!(
        "desc mean={} var={} std={}",
        b(d1.mean()),
        b(d1.variance()),
        b(d1.std_dev())
    );
    println!(
        "desc pop_var={} pop_std={} rms={}",
        b(d1.population_variance()),
        b(d1.population_std_dev()),
        b(d1.quadratic_mean())
    );
    println!(
        "desc geo_mean={} harm_mean={}",
        b(pos.geometric_mean()),
        b(pos.harmonic_mean())
    );
    println!(
        "desc cov={} pop_cov={}",
        b(d1.covariance(d2)),
        b(d1.population_covariance(d2))
    );

    // statrs 0.18 removed data-level skewness/kurtosis -> compute the sample central moments by hand
    let n1 = d1.len() as f64;
    let m = d1.mean();
    let (mut c2, mut c3, mut c4) = (0.0f64, 0.0f64, 0.0f64);
    for &x in &d1 {
        let t = x - m;
        c2 += t * t;
        c3 += t * t * t;
        c4 += t * t * t * t;
    }
    c2 /= n1;
    c3 /= n1;
    c4 /= n1;
    println!(
        "desc skewness_g1={} kurtosis_excess_g2={}",
        b(c3 / (c2.sqrt() * c2)),
        b(c4 / (c2 * c2) - 3.0)
    );

    // OrderStatistics (Data wrapper, in-place selection; median clashes with the Median trait -> UFCS)
    let mut od = Data::new(d1);
    println!(
        "ord median={} q1={} q3={} iqr={}",
        b(OrderStatistics::median(&mut od)),
        b(od.lower_quartile()),
        b(od.upper_quartile()),
        b(od.interquartile_range())
    );
    for p in [0usize, 25, 50, 75, 100] {
        println!("ord percentile({p})={}", b(od.percentile(p)));
    }
    for tau in [0.1f64, 0.9] {
        println!("ord quantile({tau})={}", b(od.quantile(tau)));
    }
    println!(
        "ord order_statistic(1)={} order_statistic(16)={}",
        b(od.order_statistic(1)),
        b(od.order_statistic(16))
    );
    // Boundary: out of range -> statrs explicitly returns the f64::NAN constant (not a computed NaN)
    println!(
        "ord edge os(0)={} os(17)={} pct(105)={} quantile(-0.5)={}",
        b(od.order_statistic(0)),
        b(od.order_statistic(17)),
        b(od.percentile(105)),
        b(od.quantile(-0.5))
    );
    let mut empty = Data::new(Vec::<f64>::new());
    println!("ord edge empty_median={}", b(OrderStatistics::median(&mut empty)));

    let tied = [2.0f64, 1.0, 2.0, 3.0, 1.0, 2.0];
    for tb in [
        RankTieBreaker::Average,
        RankTieBreaker::Min,
        RankTieBreaker::Max,
        RankTieBreaker::First,
    ] {
        let ranks = Data::new(tied).ranks(tb);
        let rs: Vec<String> = ranks.iter().map(|&v| b(v)).collect();
        println!("ord ranks {tb:?} = [{}]", rs.join(" "));
    }

    // ===== ② distribution families =====
    let nrm = Normal::new(1.0, 2.0).unwrap();
    moments("normal", &nrm);
    probe_cont("normal", &nrm, &[-3.0, 1.0, 4.5], &[0.025, 0.5, 0.975]);
    println!(
        "normal median={} mode={} min={} max={}",
        b(nrm.median()),
        ob(nrm.mode()),
        b(nrm.min()),
        b(nrm.max())
    );

    let gm = Gamma::new(3.0, 0.5).unwrap();
    moments("gamma", &gm);
    probe_cont("gamma", &gm, &[0.5, 3.0, 12.0], &[0.05, 0.5, 0.95]);

    let bt = Beta::new(2.0, 5.0).unwrap();
    moments("beta", &bt);
    probe_cont("beta", &bt, &[0.1, 0.3, 0.8], &[0.05, 0.5, 0.95]);

    let st = StudentsT::new(0.0, 1.0, 8.0).unwrap();
    moments("studentst", &st);
    probe_cont("studentst", &st, &[-2.5, 0.0, 1.75], &[0.025, 0.5, 0.975]);

    let cs = ChiSquared::new(4.0).unwrap();
    moments("chisq", &cs);
    probe_cont("chisq", &cs, &[0.5, 4.0, 9.5], &[0.05, 0.5, 0.95]);

    let ex = Exp::new(1.5).unwrap();
    moments("exp", &ex);
    probe_cont("exp", &ex, &[0.1, 1.0, 3.0], &[0.05, 0.5, 0.95]);

    let un = Uniform::new(-2.0, 3.5).unwrap();
    moments("uniform", &un);
    probe_cont("uniform", &un, &[-3.0, -2.0, 0.75, 3.5, 4.0], &[0.0, 0.25, 1.0]);
    println!("uniform min={} max={}", b(un.min()), b(un.max()));

    let bn = Binomial::new(0.3, 20).unwrap();
    moments("binomial", &bn);
    probe_disc("binomial", &bn, &[0, 6, 12, 20], &[0.05, 0.5, 0.95]);
    println!(
        "binomial mode={} min={} max={}",
        ou(bn.mode()),
        bn.min(),
        bn.max()
    );

    let po = Poisson::new(4.0).unwrap();
    moments("poisson", &po);
    probe_disc("poisson", &po, &[0, 4, 9], &[0.05, 0.5, 0.95]);
    println!("poisson median={} mode={}", b(po.median()), ou(po.mode()));

    // ===== ③ parameter error paths (static Display strings) =====
    println!("err normal(nan_mean) = {}", Normal::new(f64::NAN, 1.0).unwrap_err());
    println!("err normal(neg_sigma) = {}", Normal::new(0.0, -1.0).unwrap_err());
    println!("err normal(zero_sigma) = {}", Normal::new(0.0, 0.0).unwrap_err());
    println!("err gamma(neg_shape) = {}", Gamma::new(-1.0, 1.0).unwrap_err());
    println!("err gamma(zero_rate) = {}", Gamma::new(1.0, 0.0).unwrap_err());
    println!(
        "err gamma(inf,inf) = {}",
        Gamma::new(f64::INFINITY, f64::INFINITY).unwrap_err()
    );
    println!("err beta(zero_a) = {}", Beta::new(0.0, 1.0).unwrap_err());
    println!("err beta(nan_b) = {}", Beta::new(1.0, f64::NAN).unwrap_err());
    println!(
        "err studentst(neg_freedom) = {}",
        StudentsT::new(0.0, 1.0, -1.0).unwrap_err()
    );
    println!(
        "err studentst(nan_location) = {}",
        StudentsT::new(f64::NAN, 1.0, 1.0).unwrap_err()
    );
    println!("err chisq(zero_freedom) = {}", ChiSquared::new(0.0).unwrap_err());
    println!("err binomial(p>1) = {}", Binomial::new(1.5, 10).unwrap_err());
    println!("err binomial(nan_p) = {}", Binomial::new(f64::NAN, 10).unwrap_err());
    println!("err poisson(zero_lambda) = {}", Poisson::new(0.0).unwrap_err());
    println!("err poisson(neg_lambda) = {}", Poisson::new(-2.0).unwrap_err());
    println!("err exp(neg_rate) = {}", Exp::new(-1.0).unwrap_err());
    println!("err uniform(max<min) = {}", Uniform::new(3.0, 2.0).unwrap_err());
    println!("err uniform(nan_min) = {}", Uniform::new(f64::NAN, 1.0).unwrap_err());
    println!(
        "err uniform(inf_max) = {}",
        Uniform::new(0.0, f64::INFINITY).unwrap_err()
    );

    // ===== ④ hypothesis tests =====
    // z-test: hand-computed statistic + standard normal sf (two-sided p)
    let stdn = Normal::new(0.0, 1.0).unwrap();
    let mu0 = 2.0;
    let sigma = 3.5;
    let z = (d1.mean() - mu0) / (sigma / n1.sqrt());
    println!("ztest z={} p_two_sided={}", b(z), b(2.0 * stdn.sf(z.abs())));

    // t-test: sample std_dev + StudentsT(n-1) sf (two-sided p)
    let t = (d1.mean() - mu0) / (d1.std_dev() / n1.sqrt());
    let tdist = StudentsT::new(0.0, 1.0, n1 - 1.0).unwrap();
    println!(
        "ttest t={} df={} p_two_sided={}",
        b(t),
        n1 - 1.0,
        b(2.0 * tdist.sf(t.abs()))
    );

    // Chi-squared goodness of fit: observed counts vs Poisson(2.5) expectations (the last bucket
    // pools the tail P(K>=7)), df = bins-1, p = ChiSquared(df).sf(chi2)
    let observed = [6u64, 10, 9, 7, 4, 2, 1, 1];
    let total: u64 = observed.iter().sum();
    let poi2 = Poisson::new(2.5).unwrap();
    let mut chi2 = 0.0f64;
    for (k, &o) in observed.iter().enumerate().take(7) {
        let e = total as f64 * poi2.pmf(k as u64);
        let d = o as f64 - e;
        chi2 += d * d / e;
    }
    {
        let e = total as f64 * poi2.sf(6);
        let d = observed[7] as f64 - e;
        chi2 += d * d / e;
    }
    let gof = ChiSquared::new(observed.len() as f64 - 1.0).unwrap();
    println!(
        "chigof chi2={} df={} p={}",
        b(chi2),
        observed.len() - 1,
        b(gof.sf(chi2))
    );

    // The crate's own fisher exact test (Hypergeometric kernel) + odds ratio + zero-row boundary
    let table = [3u64, 5, 4, 50];
    for alt in [Alternative::Less, Alternative::Greater, Alternative::TwoSided] {
        let p = stats_tests::fishers_exact(&table, alt).unwrap();
        println!("fisher {alt:?} p={}", b(p));
    }
    let (odds, or_p) =
        stats_tests::fishers_exact_with_odds_ratio(&table, Alternative::TwoSided).unwrap();
    println!("fisher odds_ratio={} p={}", b(odds), b(or_p));
    let (z_odds, z_p) =
        stats_tests::fishers_exact_with_odds_ratio(&[0, 5, 0, 50], Alternative::TwoSided).unwrap();
    println!("fisher zero_row odds={} p={}", b(z_odds), b(z_p));

    // ===== ⑤ special functions =====
    println!(
        "fn gamma(5.5)={} ln_gamma(10.25)={} digamma(2.5)={}",
        b(gamma::gamma(5.5)),
        b(gamma::ln_gamma(10.25)),
        b(gamma::digamma(2.5))
    );
    println!(
        "fn beta(2.5,3.5)={} ln_beta(2.5,3.5)={} beta_reg(2,5,0.3)={}",
        b(beta::beta(2.5, 3.5)),
        b(beta::ln_beta(2.5, 3.5)),
        b(beta::beta_reg(2.0, 5.0, 0.3))
    );
    println!(
        "fn erf(0.75)={} erfc(0.75)={} erf_inv(0.5)={}",
        b(erf::erf(0.75)),
        b(erf::erfc(0.75)),
        b(erf::erf_inv(0.5))
    );
    println!(
        "fn factorial(10)={} binomial(10,4)={}",
        b(factorial::factorial(10)),
        b(factorial::binomial(10, 4))
    );
    match beta::checked_beta(-1.0, 2.0) {
        Ok(_) => println!("fn checked_beta(-1,2) unexpected_ok"),
        Err(e) => println!("fn checked_beta(-1,2) err = {e}"),
    }
}
