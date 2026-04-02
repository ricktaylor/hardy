use super::*;
use dashmap::DashMap;
use metrics::{Counter, Gauge, Histogram, Key, KeyName, Metadata, Recorder, SharedString, Unit};
use opentelemetry::metrics::{
    Counter as OtelCounter, Gauge as OtelGauge, Histogram as OtelHistogram, Meter,
};
use std::{
    borrow::Cow,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

/// A `metrics::Recorder` that forwards metrics to an OpenTelemetry `Meter`.
///
/// This recorder lazily creates OpenTelemetry instruments (counters, gauges, histograms)
/// the first time they are used and caches them for subsequent calls.
#[derive(Debug)]
pub struct OpenTelemetryRecorder {
    meter: Meter,
    counter_descs: DashMap<KeyName, (Option<Unit>, SharedString)>,
    counters: DashMap<u64, Arc<InnerCounter>>,
    gauge_descs: DashMap<KeyName, (Option<Unit>, SharedString)>,
    gauges: DashMap<u64, Arc<InnerGauge>>,
    histogram_descs: DashMap<KeyName, (Option<Unit>, SharedString)>,
    histograms: DashMap<u64, Arc<InnerHistogram>>,
}

impl OpenTelemetryRecorder {
    /// Creates a new `OtelRecorder` that will create instruments using the provided
    /// OpenTelemetry `Meter`.
    pub fn new(meter: Meter) -> Self {
        OpenTelemetryRecorder {
            meter,
            counter_descs: DashMap::new(),
            counters: DashMap::new(),
            gauge_descs: DashMap::new(),
            gauges: DashMap::new(),
            histogram_descs: DashMap::new(),
            histograms: DashMap::new(),
        }
    }
}

/// Map `metrics::Unit` strings to OTEL-compatible [UCUM](https://ucum.org/ucum) unit strings.
///
/// The `metrics` crate uses human-readable names ("seconds", "bytes") while the
/// OpenTelemetry specification expects UCUM codes ("s", "By"). Unknown units are
/// passed through as-is.
fn otel_unit(unit: &Unit) -> Cow<'static, str> {
    match unit.as_str() {
        "count" => "1".into(),
        "percent" => "%".into(),
        "seconds" => "s".into(),
        "milliseconds" => "ms".into(),
        "microseconds" => "us".into(),
        "nanoseconds" => "ns".into(),
        "bytes" => "By".into(),
        "kibibytes" => "KiBy".into(),
        "mebibytes" => "MiBy".into(),
        "gibibytes" => "GiBy".into(),
        "tebibytes" => "TiBy".into(),
        "bits_per_second" => "bit/s".into(),
        "kilobits_per_second" => "kbit/s".into(),
        "megabits_per_second" => "Mbit/s".into(),
        "gigabits_per_second" => "Gbit/s".into(),
        "terabits_per_second" => "Tbit/s".into(),
        "count_per_second" => "1/s".into(),
        other => other.into(),
    }
}

impl Recorder for OpenTelemetryRecorder {
    fn describe_counter(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        self.counter_descs.insert(key, (unit, description));
    }

    fn describe_gauge(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        self.gauge_descs.insert(key, (unit, description));
    }

    fn describe_histogram(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        self.histogram_descs.insert(key, (unit, description));
    }

    fn register_counter(&self, key: &Key, _metadata: &Metadata<'_>) -> Counter {
        Counter::from_arc(
            self.counters
                .entry(key.get_hash())
                .or_insert_with(|| {
                    let mut counter = self.meter.u64_counter(key.name().to_owned());
                    if let Some(desc) = self.counter_descs.get(key.name()) {
                        let (unit, description) = desc.value();
                        if let Some(u) = unit {
                            counter = counter.with_unit(otel_unit(u));
                        }
                        if !description.is_empty() {
                            counter = counter.with_description(description.clone());
                        }
                    }
                    Arc::new(InnerCounter {
                        counter: counter.build(),
                        labels: key
                            .labels()
                            .map(|label| {
                                KeyValue::new(label.key().to_owned(), label.value().to_owned())
                            })
                            .collect(),
                    })
                })
                .value()
                .clone(),
        )
    }

    fn register_gauge(&self, key: &Key, _metadata: &Metadata<'_>) -> Gauge {
        Gauge::from_arc(
            self.gauges
                .entry(key.get_hash())
                .or_insert_with(|| {
                    let mut gauge = self.meter.f64_gauge(key.name().to_owned());
                    if let Some(desc) = self.gauge_descs.get(key.name()) {
                        let (unit, description) = desc.value();
                        if let Some(u) = unit {
                            gauge = gauge.with_unit(otel_unit(u));
                        }
                        if !description.is_empty() {
                            gauge = gauge.with_description(description.clone());
                        }
                    }
                    Arc::new(InnerGauge {
                        gauge: gauge.build(),
                        labels: key
                            .labels()
                            .map(|label| {
                                KeyValue::new(label.key().to_owned(), label.value().to_owned())
                            })
                            .collect(),
                        current: AtomicU64::new(0f64.to_bits()),
                    })
                })
                .value()
                .clone(),
        )
    }

    fn register_histogram(&self, key: &Key, _metadata: &Metadata<'_>) -> Histogram {
        Histogram::from_arc(
            self.histograms
                .entry(key.get_hash())
                .or_insert_with(|| {
                    let mut histogram = self.meter.f64_histogram(key.name().to_owned());
                    if let Some(desc) = self.histogram_descs.get(key.name()) {
                        let (unit, description) = desc.value();
                        if let Some(u) = unit {
                            histogram = histogram.with_unit(otel_unit(u));
                        }
                        if !description.is_empty() {
                            histogram = histogram.with_description(description.clone());
                        }
                    }
                    Arc::new(InnerHistogram {
                        histogram: histogram.build(),
                        labels: key
                            .labels()
                            .map(|label| {
                                KeyValue::new(label.key().to_owned(), label.value().to_owned())
                            })
                            .collect(),
                    })
                })
                .value()
                .clone(),
        )
    }
}

#[derive(Debug)]
struct InnerCounter {
    counter: OtelCounter<u64>,
    labels: Vec<KeyValue>,
}

impl metrics::CounterFn for InnerCounter {
    fn increment(&self, value: u64) {
        self.counter.add(value, &self.labels);
    }

    fn absolute(&self, _value: u64) {
        unimplemented!(
            "absolute() is not supported; OpenTelemetry counters are monotonic and can only be incremented"
        )
    }
}

#[derive(Debug)]
struct InnerGauge {
    gauge: OtelGauge<f64>,
    labels: Vec<KeyValue>,
    current: AtomicU64, // stores f64 bits via to_bits()/from_bits()
}

impl InnerGauge {
    fn update_and_record(&self, f: impl Fn(f64) -> f64) {
        let new_val = loop {
            let bits = self.current.load(Ordering::Relaxed);
            let new_val = f(f64::from_bits(bits));
            if self
                .current
                .compare_exchange_weak(
                    bits,
                    new_val.to_bits(),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                break new_val;
            }
        };
        self.gauge.record(new_val, &self.labels);
    }
}

impl metrics::GaugeFn for InnerGauge {
    fn increment(&self, value: f64) {
        self.update_and_record(|current| current + value);
    }

    fn decrement(&self, value: f64) {
        self.update_and_record(|current| current - value);
    }

    fn set(&self, value: f64) {
        self.current.store(value.to_bits(), Ordering::Relaxed);
        self.gauge.record(value, &self.labels);
    }
}

#[derive(Debug)]
struct InnerHistogram {
    histogram: OtelHistogram<f64>,
    labels: Vec<KeyValue>,
}

impl metrics::HistogramFn for InnerHistogram {
    fn record(&self, value: f64) {
        self.histogram.record(value, &self.labels);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use metrics::{CounterFn, GaugeFn, HistogramFn};
    use opentelemetry_sdk::metrics::SdkMeterProvider;

    fn test_meter() -> Meter {
        SdkMeterProvider::builder().build().meter("test")
    }

    fn make_gauge(meter: &Meter) -> InnerGauge {
        InnerGauge {
            gauge: meter.f64_gauge("test_gauge").build(),
            labels: vec![],
            current: AtomicU64::new(0f64.to_bits()),
        }
    }

    fn gauge_value(g: &InnerGauge) -> f64 {
        f64::from_bits(g.current.load(Ordering::Relaxed))
    }

    // -- InnerGauge tests --

    #[test]
    fn gauge_set() {
        let g = make_gauge(&test_meter());
        g.set(42.0);
        assert_eq!(gauge_value(&g), 42.0);
    }

    #[test]
    fn gauge_increment() {
        let g = make_gauge(&test_meter());
        g.increment(1.0);
        g.increment(2.5);
        assert_eq!(gauge_value(&g), 3.5);
    }

    #[test]
    fn gauge_decrement() {
        let g = make_gauge(&test_meter());
        g.set(10.0);
        g.decrement(3.0);
        assert_eq!(gauge_value(&g), 7.0);
    }

    #[test]
    fn gauge_increment_decrement_sequence() {
        let g = make_gauge(&test_meter());
        g.increment(1.0);
        g.increment(1.0);
        g.increment(1.0);
        g.decrement(1.0);
        assert_eq!(gauge_value(&g), 2.0);
    }

    #[test]
    fn gauge_set_overrides_accumulated() {
        let g = make_gauge(&test_meter());
        g.increment(5.0);
        g.set(100.0);
        assert_eq!(gauge_value(&g), 100.0);
    }

    #[test]
    fn gauge_decrement_below_zero() {
        let g = make_gauge(&test_meter());
        g.decrement(1.0);
        assert_eq!(gauge_value(&g), -1.0);
    }

    #[test]
    fn gauge_with_labels() {
        let meter = test_meter();
        let g = InnerGauge {
            gauge: meter.f64_gauge("labeled_gauge").build(),
            labels: vec![KeyValue::new("env", "test")],
            current: AtomicU64::new(0f64.to_bits()),
        };
        g.increment(1.0);
        assert_eq!(gauge_value(&g), 1.0);
    }

    // -- InnerCounter tests --

    #[test]
    fn counter_increment() {
        let meter = test_meter();
        let c = InnerCounter {
            counter: meter.u64_counter("test_counter").build(),
            labels: vec![],
        };
        // Counter is fire-and-forget (no readable state), but verify it doesn't panic
        c.increment(1);
        c.increment(100);
    }

    #[test]
    #[should_panic(expected = "absolute() is not supported")]
    fn counter_absolute_panics() {
        let meter = test_meter();
        let c = InnerCounter {
            counter: meter.u64_counter("test_counter").build(),
            labels: vec![],
        };
        c.absolute(42);
    }

    // -- InnerHistogram tests --

    #[test]
    fn histogram_record() {
        let meter = test_meter();
        let h = InnerHistogram {
            histogram: meter.f64_histogram("test_histogram").build(),
            labels: vec![],
        };
        // Fire-and-forget, verify no panic
        h.record(1.5);
        h.record(100.0);
    }

    // -- OpenTelemetryRecorder tests --

    #[test]
    fn recorder_register_gauge_and_use() {
        let recorder = OpenTelemetryRecorder::new(test_meter());
        let key = Key::from_name("rec_test_gauge");
        let metadata = metrics::Metadata::new(module_path!(), metrics::Level::INFO, None);
        let gauge = recorder.register_gauge(&key, &metadata);

        // Should not panic — increment is now implemented
        gauge.increment(5.0);
        gauge.increment(3.0);
        gauge.decrement(2.0);

        // Verify cached: second register returns same instrument
        let gauge2 = recorder.register_gauge(&key, &metadata);
        gauge2.increment(1.0);

        // Both point to the same InnerGauge, so value should be 5+3-2+1 = 7
        let inner = recorder.gauges.get(&key.get_hash()).unwrap();
        assert_eq!(f64::from_bits(inner.current.load(Ordering::Relaxed)), 7.0);
    }

    #[test]
    fn recorder_describe_then_register() {
        let recorder = OpenTelemetryRecorder::new(test_meter());

        // Describe before register (the normal pattern)
        recorder.describe_gauge(
            "described_gauge".into(),
            Some(Unit::Count),
            "A test gauge".into(),
        );
        recorder.describe_counter(
            "described_counter".into(),
            Some(Unit::Count),
            "A test counter".into(),
        );
        recorder.describe_histogram(
            "described_histogram".into(),
            Some(Unit::Seconds),
            "A test histogram".into(),
        );

        let metadata = metrics::Metadata::new(module_path!(), metrics::Level::INFO, None);

        // Register should pick up descriptions without panicking
        let gauge = recorder.register_gauge(&Key::from_name("described_gauge"), &metadata);
        gauge.set(1.0);

        let counter = recorder.register_counter(&Key::from_name("described_counter"), &metadata);
        counter.increment(1);

        let histogram =
            recorder.register_histogram(&Key::from_name("described_histogram"), &metadata);
        histogram.record(0.5);
    }

    #[test]
    fn recorder_labeled_gauge() {
        let recorder = OpenTelemetryRecorder::new(test_meter());
        let metadata = metrics::Metadata::new(module_path!(), metrics::Level::INFO, None);

        let key = Key::from_parts("labeled_gauge", vec![metrics::Label::new("env", "prod")]);
        let gauge = recorder.register_gauge(&key, &metadata);
        gauge.increment(1.0);

        let inner = recorder.gauges.get(&key.get_hash()).unwrap();
        assert_eq!(inner.labels.len(), 1);
        assert_eq!(inner.labels[0].key.as_str(), "env");
        assert_eq!(inner.labels[0].value.as_str(), "prod");
        assert_eq!(f64::from_bits(inner.current.load(Ordering::Relaxed)), 1.0);
    }

    // -- Macro-driven tests (using with_local_recorder) --
    //
    // These test the full path that BPA code uses:
    //   metrics::counter!() / gauge!() / histogram!()
    //     → global/local recorder lookup
    //       → OpenTelemetryRecorder::register_*()
    //         → InnerCounter/InnerGauge/InnerHistogram

    /// Helper: look up the gauge's tracked value from the recorder's cache.
    /// The metrics macros use Key hashing internally, so we reconstruct the
    /// key the same way the macro would to find the cached instrument.
    fn recorder_gauge_value(recorder: &OpenTelemetryRecorder, name: &str) -> f64 {
        let key = Key::from_name(name.to_string());
        let inner = recorder
            .gauges
            .get(&key.get_hash())
            .expect("gauge not found in recorder cache");
        f64::from_bits(inner.current.load(Ordering::Relaxed))
    }

    #[test]
    fn macro_gauge_increment_decrement() {
        let recorder = OpenTelemetryRecorder::new(test_meter());
        metrics::with_local_recorder(&recorder, || {
            metrics::gauge!("macro_gauge").increment(1.0);
            metrics::gauge!("macro_gauge").increment(1.0);
            metrics::gauge!("macro_gauge").increment(1.0);
            metrics::gauge!("macro_gauge").decrement(1.0);
        });
        assert_eq!(recorder_gauge_value(&recorder, "macro_gauge"), 2.0);
    }

    #[test]
    fn macro_gauge_set() {
        let recorder = OpenTelemetryRecorder::new(test_meter());
        metrics::with_local_recorder(&recorder, || {
            metrics::gauge!("macro_set_gauge").set(42.0);
        });
        assert_eq!(recorder_gauge_value(&recorder, "macro_set_gauge"), 42.0);
    }

    #[test]
    fn macro_gauge_set_overrides_increments() {
        let recorder = OpenTelemetryRecorder::new(test_meter());
        metrics::with_local_recorder(&recorder, || {
            metrics::gauge!("macro_override").increment(10.0);
            metrics::gauge!("macro_override").set(0.0);
        });
        assert_eq!(recorder_gauge_value(&recorder, "macro_override"), 0.0);
    }

    #[test]
    fn macro_gauge_with_labels() {
        let recorder = OpenTelemetryRecorder::new(test_meter());
        metrics::with_local_recorder(&recorder, || {
            metrics::gauge!("macro_labeled", "reason" => "test").increment(5.0);
            metrics::gauge!("macro_labeled", "reason" => "test").decrement(2.0);
        });
        // Labeled gauges get a different hash than unlabeled, so look up via Key::from_parts
        let key = Key::from_parts("macro_labeled", vec![metrics::Label::new("reason", "test")]);
        let inner = recorder.gauges.get(&key.get_hash()).unwrap();
        assert_eq!(f64::from_bits(inner.current.load(Ordering::Relaxed)), 3.0);
    }

    #[test]
    fn macro_counter() {
        let recorder = OpenTelemetryRecorder::new(test_meter());
        metrics::with_local_recorder(&recorder, || {
            metrics::counter!("macro_counter").increment(1);
            metrics::counter!("macro_counter").increment(99);
        });
        // Counter exists in cache (doesn't panic, was registered)
        let key = Key::from_name("macro_counter");
        assert!(recorder.counters.contains_key(&key.get_hash()));
    }

    #[test]
    fn macro_counter_with_labels() {
        let recorder = OpenTelemetryRecorder::new(test_meter());
        metrics::with_local_recorder(&recorder, || {
            metrics::counter!("macro_labeled_ctr", "reason" => "expired").increment(1);
            metrics::counter!("macro_labeled_ctr", "reason" => "expired").increment(1);
        });
        let key = Key::from_parts(
            "macro_labeled_ctr",
            vec![metrics::Label::new("reason", "expired")],
        );
        assert!(recorder.counters.contains_key(&key.get_hash()));
    }

    #[test]
    fn macro_histogram() {
        let recorder = OpenTelemetryRecorder::new(test_meter());
        metrics::with_local_recorder(&recorder, || {
            metrics::histogram!("macro_histogram").record(1.5);
            metrics::histogram!("macro_histogram").record(100.0);
        });
        let key = Key::from_name("macro_histogram");
        assert!(recorder.histograms.contains_key(&key.get_hash()));
    }

    #[test]
    fn macro_histogram_with_labels() {
        let recorder = OpenTelemetryRecorder::new(test_meter());
        metrics::with_local_recorder(&recorder, || {
            metrics::histogram!("macro_labeled_hist", "endpoint" => "/api").record(0.5);
            metrics::histogram!("macro_labeled_hist", "endpoint" => "/api").record(1.2);
        });
        let key = Key::from_parts(
            "macro_labeled_hist",
            vec![metrics::Label::new("endpoint", "/api")],
        );
        let inner = recorder.histograms.get(&key.get_hash()).unwrap();
        assert_eq!(inner.labels.len(), 1);
        assert_eq!(inner.labels[0].key.as_str(), "endpoint");
        assert_eq!(inner.labels[0].value.as_str(), "/api");
    }

    #[test]
    fn macro_describe_then_use() {
        let recorder = OpenTelemetryRecorder::new(test_meter());
        metrics::with_local_recorder(&recorder, || {
            // This is the pattern BPA code uses: describe once, then use
            metrics::describe_counter!("bpa.test.received", metrics::Unit::Count, "Test counter");
            metrics::describe_gauge!("bpa.test.bundles", metrics::Unit::Count, "Test gauge");
            metrics::describe_histogram!(
                "bpa.test.latency",
                metrics::Unit::Seconds,
                "Test histogram"
            );

            metrics::counter!("bpa.test.received").increment(1);
            metrics::gauge!("bpa.test.bundles").increment(1.0);
            metrics::histogram!("bpa.test.latency").record(0.042);
        });

        // Verify descriptions were stored
        assert!(
            recorder
                .counter_descs
                .contains_key(&KeyName::from("bpa.test.received"))
        );
        assert!(
            recorder
                .gauge_descs
                .contains_key(&KeyName::from("bpa.test.bundles"))
        );
        assert!(
            recorder
                .histogram_descs
                .contains_key(&KeyName::from("bpa.test.latency"))
        );

        // Verify gauge value tracked correctly
        assert_eq!(recorder_gauge_value(&recorder, "bpa.test.bundles"), 1.0);
    }

    #[test]
    fn macro_use_without_describe() {
        // Exercises the "no description registered" path in register_*(),
        // covering the branches where counter_descs/gauge_descs/histogram_descs
        // lookups return None.
        let recorder = OpenTelemetryRecorder::new(test_meter());
        metrics::with_local_recorder(&recorder, || {
            metrics::counter!("undescribed_counter").increment(1);
            metrics::gauge!("undescribed_gauge").increment(1.0);
            metrics::histogram!("undescribed_histogram").record(0.5);
        });
        assert_eq!(recorder_gauge_value(&recorder, "undescribed_gauge"), 1.0);
    }

    // -- Unit mapping tests --

    #[test]
    fn otel_unit_mapping() {
        assert_eq!(otel_unit(&Unit::Count), "1");
        assert_eq!(otel_unit(&Unit::Percent), "%");
        assert_eq!(otel_unit(&Unit::Seconds), "s");
        assert_eq!(otel_unit(&Unit::Milliseconds), "ms");
        assert_eq!(otel_unit(&Unit::Microseconds), "us");
        assert_eq!(otel_unit(&Unit::Nanoseconds), "ns");
        assert_eq!(otel_unit(&Unit::Bytes), "By");
        assert_eq!(otel_unit(&Unit::Kibibytes), "KiBy");
        assert_eq!(otel_unit(&Unit::Mebibytes), "MiBy");
        assert_eq!(otel_unit(&Unit::Gibibytes), "GiBy");
        assert_eq!(otel_unit(&Unit::Tebibytes), "TiBy");
        assert_eq!(otel_unit(&Unit::BitsPerSecond), "bit/s");
        assert_eq!(otel_unit(&Unit::KilobitsPerSecond), "kbit/s");
        assert_eq!(otel_unit(&Unit::MegabitsPerSecond), "Mbit/s");
        assert_eq!(otel_unit(&Unit::GigabitsPerSecond), "Gbit/s");
        assert_eq!(otel_unit(&Unit::TerabitsPerSecond), "Tbit/s");
        assert_eq!(otel_unit(&Unit::CountPerSecond), "1/s");
    }

    #[test]
    fn macro_multiple_label_values_are_distinct() {
        let recorder = OpenTelemetryRecorder::new(test_meter());
        metrics::with_local_recorder(&recorder, || {
            // Same metric name, different label values → different instruments
            metrics::gauge!("multi_label", "reason" => "a").increment(1.0);
            metrics::gauge!("multi_label", "reason" => "b").increment(10.0);
        });

        let key_a = Key::from_parts("multi_label", vec![metrics::Label::new("reason", "a")]);
        let key_b = Key::from_parts("multi_label", vec![metrics::Label::new("reason", "b")]);

        let inner_a = recorder.gauges.get(&key_a.get_hash()).unwrap();
        let inner_b = recorder.gauges.get(&key_b.get_hash()).unwrap();
        assert_eq!(f64::from_bits(inner_a.current.load(Ordering::Relaxed)), 1.0);
        assert_eq!(
            f64::from_bits(inner_b.current.load(Ordering::Relaxed)),
            10.0
        );
    }
}
