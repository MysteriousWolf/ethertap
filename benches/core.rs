use criterion::{black_box, criterion_group, criterion_main, Criterion};

fn bench_bpm_to_float(c: &mut Criterion) {
    c.bench_function("bpm_to_float", |b| {
        b.iter(|| ethertap::osc::bpm_to_float(black_box(120.0)))
    });
}

fn bench_float_to_bpm(c: &mut Criterion) {
    c.bench_function("float_to_bpm", |b| {
        b.iter(|| ethertap::osc::float_to_bpm(black_box(0.1667)))
    });
}

fn bench_heartbeat(c: &mut Criterion) {
    c.bench_function("osc_heartbeat", |b| b.iter(|| ethertap::osc::heartbeat()));
}

fn bench_set_fx_delay(c: &mut Criterion) {
    c.bench_function("osc_set_fx_delay", |b| {
        b.iter(|| ethertap::osc::set_fx_delay(black_box(1), black_box(10), black_box(0.1667)))
    });
}

fn bench_set_fx_delay_modd(c: &mut Criterion) {
    c.bench_function("osc_set_fx_delay_modd", |b| {
        b.iter(|| ethertap::osc::set_fx_delay(black_box(3), black_box(26), black_box(0.25)))
    });
}

fn bench_query_fx_type(c: &mut Criterion) {
    c.bench_function("osc_query_fx_type", |b| {
        b.iter(|| ethertap::osc::query_fx_type(black_box(1)))
    });
}

fn bench_query_fx_delay(c: &mut Criterion) {
    c.bench_function("osc_query_fx_delay", |b| {
        b.iter(|| ethertap::osc::query_fx_delay(black_box(2), black_box(10)))
    });
}

fn bench_set_fxrtn_mute(c: &mut Criterion) {
    c.bench_function("osc_set_fxrtn_mute", |b| {
        b.iter(|| ethertap::osc::set_fxrtn_mute(black_box(3), black_box(true)))
    });
}

fn bench_is_bpm_compatible(c: &mut Criterion) {
    c.bench_function("is_bpm_compatible", |b| {
        b.iter(|| {
            ethertap::osc::is_bpm_compatible(black_box(10), black_box(1));
            ethertap::osc::is_bpm_compatible(black_box(21), black_box(4));
            ethertap::osc::is_bpm_compatible(black_box(1), black_box(2));
            ethertap::osc::is_bpm_compatible(black_box(99), black_box(1));
        })
    });
}

fn bench_delay_par(c: &mut Criterion) {
    c.bench_function("delay_par", |b| {
        b.iter(|| {
            ethertap::osc::delay_par(black_box(10));
            ethertap::osc::delay_par(black_box(11));
            ethertap::osc::delay_par(black_box(26));
            ethertap::osc::delay_par(black_box(0));
        })
    });
}

fn bench_fx_type_short(c: &mut Criterion) {
    c.bench_function("fx_type_short", |b| {
        b.iter(|| {
            ethertap::osc::fx_type_short(black_box(0), black_box(1));
            ethertap::osc::fx_type_short(black_box(10), black_box(1));
            ethertap::osc::fx_type_short(black_box(60), black_box(2));
        })
    });
}

fn bench_fx_type_long(c: &mut Criterion) {
    c.bench_function("fx_type_long", |b| {
        b.iter(|| {
            ethertap::osc::fx_type_long(black_box(0), black_box(1));
            ethertap::osc::fx_type_long(black_box(10), black_box(1));
            ethertap::osc::fx_type_long(black_box(26), black_box(3));
        })
    });
}

fn bench_delay_par_all_compatible(c: &mut Criterion) {
    c.bench_function("delay_par_all_compatible", |b| {
        let types = [10, 11, 12, 21, 24, 25, 26];
        b.iter(|| {
            for &t in &types {
                black_box(ethertap::osc::delay_par(black_box(t)));
            }
        })
    });
}

fn bench_bpm_roundtrip_sweep(c: &mut Criterion) {
    c.bench_function("bpm_roundtrip_sweep", |b| {
        let bpms: Vec<f64> = (30..=240).map(|b| b as f64).collect();
        b.iter(|| {
            for &bpm in &bpms {
                let f = ethertap::osc::bpm_to_float(black_box(bpm));
                black_box(ethertap::osc::float_to_bpm(f));
            }
        })
    });
}

criterion_group!(
    name = osc_build;
    config = Criterion::default().sample_size(100);
    targets =
        bench_bpm_to_float,
        bench_float_to_bpm,
        bench_heartbeat,
        bench_set_fx_delay,
        bench_set_fx_delay_modd,
        bench_query_fx_type,
        bench_query_fx_delay,
        bench_set_fxrtn_mute,
        bench_is_bpm_compatible,
        bench_delay_par,
        bench_fx_type_short,
        bench_fx_type_long,
        bench_delay_par_all_compatible,
        bench_bpm_roundtrip_sweep,
);
criterion_main!(osc_build);
