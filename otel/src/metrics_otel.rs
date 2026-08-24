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
    // TODO: The CAS update and gauge.record() are not atomic together — under concurrent
    // updates a stale value can be recorded after a newer one. The internal `current` is
    // always correct, but OTEL may briefly export a previous value. This self-corrects on
    // the next operation. Consider switching to an OTEL observable gauge (pull model) where
    // a callback reads `current` at export time, eliminating the race entirely.
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
    use std::{sync::Barrier, thread};

    use metrics::{CounterFn, GaugeFn, HistogramFn};
    use opentelemetry_sdk::metrics::{
        InMemoryMetricExporter, PeriodicReader, SdkMeterProvider,
        data::{AggregatedMetrics, Metric, MetricData, ResourceMetrics},
    };

    use super::*;

    fn test_meter() -> Meter {
        SdkMeterProvider::builder().build().meter("test")
    }

    // Builds a meter whose recordings are observable through the public
    // export pipeline via the returned in-memory exporter.
    fn exporting_meter() -> (SdkMeterProvider, InMemoryMetricExporter, Meter) {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        let meter = provider.meter("test");
        (provider, exporter, meter)
    }

    // Flushes the provider and returns everything exported so far.
    fn export(
        provider: &SdkMeterProvider,
        exporter: &InMemoryMetricExporter,
    ) -> Vec<ResourceMetrics> {
        provider.force_flush().expect("force_flush failed");
        exporter
            .get_finished_metrics()
            .expect("get_finished_metrics failed")
    }

    // Finds the metric named `name` in the last exported batch and passes it to
    // `f`; the `Metric` exposes unit/description plus the aggregated data.
    fn with_exported_metric<T>(
        finished: &[ResourceMetrics],
        name: &str,
        f: impl FnOnce(&Metric) -> T,
    ) -> T {
        f(finished
            .last()
            .expect("no metrics exported")
            .scope_metrics()
            .flat_map(|sm| sm.metrics())
            .find(|m| m.name() == name)
            .unwrap_or_else(|| panic!("metric {name} not exported")))
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

    // The CAS retry loop in update_and_record exists to prevent lost updates
    // under concurrent read-modify-write; a plain load/modify/store would pass
    // every single-threaded test but drop updates here.
    #[test]
    fn gauge_concurrent_updates_do_not_lose_any() {
        const THREADS: usize = 8;
        const OPS: usize = 1_000;

        let (provider, exporter, meter) = exporting_meter();
        let recorder = OpenTelemetryRecorder::new(meter);
        let metadata = metrics::Metadata::new(module_path!(), metrics::Level::INFO, None);
        let key = Key::from_name("concurrent_gauge");
        let gauge = recorder.register_gauge(&key, &metadata);

        let barrier = Barrier::new(THREADS);
        thread::scope(|s| {
            for i in 0..THREADS {
                let gauge = &gauge;
                let barrier = &barrier;
                s.spawn(move || {
                    barrier.wait();
                    for _ in 0..OPS {
                        if i % 2 == 0 {
                            gauge.increment(2.0);
                        } else {
                            gauge.decrement(1.0);
                        }
                    }
                });
            }
        });

        // Half the threads add 2.0 per op, half subtract 1.0 per op:
        // net = (4 * 1000 * 2.0) - (4 * 1000 * 1.0). All intermediate values
        // are small integers, so f64 arithmetic is exact.
        let expected = (THREADS / 2 * OPS) as f64 * (2.0 - 1.0);
        assert_eq!(recorder_gauge_value(&recorder, &key), expected);

        // The CAS/record pair is not atomic, so the last record during the
        // contention phase may be stale (see the TODO on update_and_record).
        // One quiescent update records the settled value, which the OTEL
        // last-value gauge aggregation then exports deterministically.
        gauge.increment(0.0);
        let finished = export(&provider, &exporter);
        with_exported_metric(&finished, "concurrent_gauge", |metric| {
            let AggregatedMetrics::F64(MetricData::Gauge(g)) = metric.data() else {
                panic!("expected an f64 gauge, got {:?}", metric.data());
            };
            let points: Vec<_> = g.data_points().collect();
            assert_eq!(points.len(), 1);
            assert_eq!(points[0].value(), expected);
        });
    }

    // -- InnerCounter tests --

    #[test]
    fn counter_increment() {
        let (provider, exporter, meter) = exporting_meter();
        let c = InnerCounter {
            counter: meter.u64_counter("test_counter").build(),
            labels: vec![],
        };
        c.increment(1);
        c.increment(100);

        // Both increments reach the OTEL instrument.
        let finished = export(&provider, &exporter);
        with_exported_metric(&finished, "test_counter", |metric| {
            let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data() else {
                panic!("expected a u64 sum, got {:?}", metric.data());
            };
            let points: Vec<_> = sum.data_points().collect();
            assert_eq!(points.len(), 1);
            assert_eq!(points[0].value(), 101);
        });
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
        let (provider, exporter, meter) = exporting_meter();
        let h = InnerHistogram {
            histogram: meter.f64_histogram("test_histogram").build(),
            labels: vec![],
        };
        h.record(1.5);
        h.record(100.0);

        // Both recordings reach the OTEL instrument.
        let finished = export(&provider, &exporter);
        with_exported_metric(&finished, "test_histogram", |metric| {
            let AggregatedMetrics::F64(MetricData::Histogram(hist)) = metric.data() else {
                panic!("expected an f64 histogram, got {:?}", metric.data());
            };
            let points: Vec<_> = hist.data_points().collect();
            assert_eq!(points.len(), 1);
            assert_eq!(points[0].count(), 2);
            assert_eq!(points[0].sum(), 101.5);
        });
    }

    // -- OpenTelemetryRecorder tests --

    #[test]
    fn recorder_register_gauge_and_use() {
        let recorder = OpenTelemetryRecorder::new(test_meter());
        let key = Key::from_name("rec_test_gauge");
        let metadata = metrics::Metadata::new(module_path!(), metrics::Level::INFO, None);
        let gauge = recorder.register_gauge(&key, &metadata);

        gauge.increment(5.0);
        gauge.increment(3.0);
        gauge.decrement(2.0);

        // Verify cached: second register returns same instrument
        let gauge2 = recorder.register_gauge(&key, &metadata);
        gauge2.increment(1.0);

        // Both point to the same InnerGauge, so value should be 5+3-2+1 = 7
        assert_eq!(recorder_gauge_value(&recorder, &key), 7.0);
    }

    #[test]
    fn recorder_caches_counters_and_histograms() {
        let recorder = OpenTelemetryRecorder::new(test_meter());
        let metadata = metrics::Metadata::new(module_path!(), metrics::Level::INFO, None);

        // Registering the same key twice returns the cached instrument rather
        // than building and storing a fresh one per call.
        let counter_key = Key::from_name("cached_counter");
        let _ = recorder.register_counter(&counter_key, &metadata);
        let first_counter = recorder
            .counters
            .get(&counter_key.get_hash())
            .expect("counter not cached")
            .clone();
        let _ = recorder.register_counter(&counter_key, &metadata);
        assert_eq!(recorder.counters.len(), 1);
        assert!(Arc::ptr_eq(
            &first_counter,
            &recorder.counters.get(&counter_key.get_hash()).unwrap()
        ));

        let histogram_key = Key::from_name("cached_histogram");
        let _ = recorder.register_histogram(&histogram_key, &metadata);
        let first_histogram = recorder
            .histograms
            .get(&histogram_key.get_hash())
            .expect("histogram not cached")
            .clone();
        let _ = recorder.register_histogram(&histogram_key, &metadata);
        assert_eq!(recorder.histograms.len(), 1);
        assert!(Arc::ptr_eq(
            &first_histogram,
            &recorder.histograms.get(&histogram_key.get_hash()).unwrap()
        ));
    }

    #[test]
    fn recorder_describe_then_register() {
        let (provider, exporter, meter) = exporting_meter();
        let recorder = OpenTelemetryRecorder::new(meter);

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

        recorder
            .register_gauge(&Key::from_name("described_gauge"), &metadata)
            .set(1.0);
        recorder
            .register_counter(&Key::from_name("described_counter"), &metadata)
            .increment(1);
        recorder
            .register_histogram(&Key::from_name("described_histogram"), &metadata)
            .record(0.5);

        // The describe-time unit (mapped to UCUM) and description reach the
        // built OTEL instruments and are visible on the exported metrics.
        let finished = export(&provider, &exporter);
        with_exported_metric(&finished, "described_gauge", |metric| {
            assert_eq!(metric.unit(), "1");
            assert_eq!(metric.description(), "A test gauge");
        });
        with_exported_metric(&finished, "described_counter", |metric| {
            assert_eq!(metric.unit(), "1");
            assert_eq!(metric.description(), "A test counter");
        });
        with_exported_metric(&finished, "described_histogram", |metric| {
            assert_eq!(metric.unit(), "s");
            assert_eq!(metric.description(), "A test histogram");
        });
    }

    // Instruments are built once on first register and cached by key hash, so
    // a describe arriving after that build is silently ignored for the cached
    // instrument. This pins the current contract: the `metrics` crate frames
    // describe_* as an up-front declaration, and honouring late describes
    // would require rebuilding instruments on the hot record path.
    #[test]
    fn describe_after_register_does_not_retroactively_apply() {
        let (provider, exporter, meter) = exporting_meter();
        let recorder = OpenTelemetryRecorder::new(meter);
        let metadata = metrics::Metadata::new(module_path!(), metrics::Level::INFO, None);
        let key = Key::from_name("late_described_counter");

        recorder.register_counter(&key, &metadata).increment(1);
        recorder.describe_counter(
            "late_described_counter".into(),
            Some(Unit::Bytes),
            "late".into(),
        );
        recorder.register_counter(&key, &metadata).increment(1);

        let finished = export(&provider, &exporter);
        with_exported_metric(&finished, "late_described_counter", |metric| {
            assert_eq!(metric.unit(), "");
            assert_eq!(metric.description(), "");
        });
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
        assert_eq!(gauge_value(&inner), 1.0);
    }

    // -- Macro-driven tests (using with_local_recorder) --
    //
    // These test the full path that BPA code uses:
    //   metrics::counter!() / gauge!() / histogram!()
    //     → global/local recorder lookup
    //       → OpenTelemetryRecorder::register_*()
    //         → InnerCounter/InnerGauge/InnerHistogram

    // Helper: look up the gauge's tracked value from the recorder's cache.
    // The metrics macros use Key hashing internally, so callers reconstruct
    // the key the same way the macro would to find the cached instrument.
    fn recorder_gauge_value(recorder: &OpenTelemetryRecorder, key: &Key) -> f64 {
        gauge_value(
            &recorder
                .gauges
                .get(&key.get_hash())
                .expect("gauge not found in recorder cache"),
        )
    }

    #[test]
    fn macro_gauge_increment_decrement() {
        let (provider, exporter, meter) = exporting_meter();
        let recorder = OpenTelemetryRecorder::new(meter);
        metrics::with_local_recorder(&recorder, || {
            metrics::gauge!("macro_gauge").increment(1.0);
            metrics::gauge!("macro_gauge").increment(1.0);
            metrics::gauge!("macro_gauge").increment(1.0);
            metrics::gauge!("macro_gauge").decrement(1.0);
        });
        assert_eq!(
            recorder_gauge_value(&recorder, &Key::from_name("macro_gauge")),
            2.0
        );

        // The last accumulated value is what OTEL exports.
        let finished = export(&provider, &exporter);
        with_exported_metric(&finished, "macro_gauge", |metric| {
            let AggregatedMetrics::F64(MetricData::Gauge(gauge)) = metric.data() else {
                panic!("expected an f64 gauge, got {:?}", metric.data());
            };
            let points: Vec<_> = gauge.data_points().collect();
            assert_eq!(points.len(), 1);
            assert_eq!(points[0].value(), 2.0);
        });
    }

    #[test]
    fn macro_gauge_set() {
        let (provider, exporter, meter) = exporting_meter();
        let recorder = OpenTelemetryRecorder::new(meter);
        metrics::with_local_recorder(&recorder, || {
            metrics::gauge!("macro_set_gauge").set(42.0);
        });

        let finished = export(&provider, &exporter);
        with_exported_metric(&finished, "macro_set_gauge", |metric| {
            let AggregatedMetrics::F64(MetricData::Gauge(gauge)) = metric.data() else {
                panic!("expected an f64 gauge, got {:?}", metric.data());
            };
            let points: Vec<_> = gauge.data_points().collect();
            assert_eq!(points.len(), 1);
            assert_eq!(points[0].value(), 42.0);
        });
    }

    #[test]
    fn macro_gauge_set_overrides_increments() {
        let recorder = OpenTelemetryRecorder::new(test_meter());
        metrics::with_local_recorder(&recorder, || {
            metrics::gauge!("macro_override").increment(10.0);
            metrics::gauge!("macro_override").set(0.0);
        });
        assert_eq!(
            recorder_gauge_value(&recorder, &Key::from_name("macro_override")),
            0.0
        );
    }

    #[test]
    fn macro_gauge_with_labels() {
        let (provider, exporter, meter) = exporting_meter();
        let recorder = OpenTelemetryRecorder::new(meter);
        metrics::with_local_recorder(&recorder, || {
            metrics::gauge!("macro_labeled", "reason" => "test").increment(5.0);
            metrics::gauge!("macro_labeled", "reason" => "test").decrement(2.0);
            metrics::gauge!("macro_labeled", "reason" => "test").set(10.0);
        });
        // Labeled gauges get a different hash than unlabeled, so look up via Key::from_parts
        let key = Key::from_parts("macro_labeled", vec![metrics::Label::new("reason", "test")]);
        assert_eq!(recorder_gauge_value(&recorder, &key), 10.0);

        // Both the increment/decrement and set paths record with the label,
        // so a single data point carrying the OTEL attribute is exported.
        let finished = export(&provider, &exporter);
        with_exported_metric(&finished, "macro_labeled", |metric| {
            let AggregatedMetrics::F64(MetricData::Gauge(gauge)) = metric.data() else {
                panic!("expected an f64 gauge, got {:?}", metric.data());
            };
            let points: Vec<_> = gauge.data_points().collect();
            assert_eq!(points.len(), 1);
            assert_eq!(points[0].value(), 10.0);
            let attrs: Vec<_> = points[0].attributes().collect();
            assert_eq!(attrs.len(), 1);
            assert_eq!(attrs[0].key.as_str(), "reason");
            assert_eq!(attrs[0].value.as_str(), "test");
        });
    }

    #[test]
    fn macro_counter() {
        let (provider, exporter, meter) = exporting_meter();
        let recorder = OpenTelemetryRecorder::new(meter);
        metrics::with_local_recorder(&recorder, || {
            metrics::counter!("macro_counter").increment(1);
            metrics::counter!("macro_counter").increment(99);
        });

        // Both increments reach the OTEL instrument and export as one sum.
        let finished = export(&provider, &exporter);
        with_exported_metric(&finished, "macro_counter", |metric| {
            let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data() else {
                panic!("expected a u64 sum, got {:?}", metric.data());
            };
            let points: Vec<_> = sum.data_points().collect();
            assert_eq!(points.len(), 1);
            assert_eq!(points[0].value(), 100);
        });
    }

    #[test]
    fn macro_counter_with_labels() {
        let (provider, exporter, meter) = exporting_meter();
        let recorder = OpenTelemetryRecorder::new(meter);
        metrics::with_local_recorder(&recorder, || {
            metrics::counter!("macro_labeled_ctr", "reason" => "expired").increment(1);
            metrics::counter!("macro_labeled_ctr", "reason" => "expired").increment(1);
        });

        let finished = export(&provider, &exporter);
        with_exported_metric(&finished, "macro_labeled_ctr", |metric| {
            let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data() else {
                panic!("expected a u64 sum, got {:?}", metric.data());
            };
            let points: Vec<_> = sum.data_points().collect();
            assert_eq!(points.len(), 1);
            assert_eq!(points[0].value(), 2);
            let attrs: Vec<_> = points[0].attributes().collect();
            assert_eq!(attrs.len(), 1);
            assert_eq!(attrs[0].key.as_str(), "reason");
            assert_eq!(attrs[0].value.as_str(), "expired");
        });
    }

    #[test]
    fn macro_histogram() {
        let (provider, exporter, meter) = exporting_meter();
        let recorder = OpenTelemetryRecorder::new(meter);
        metrics::with_local_recorder(&recorder, || {
            metrics::histogram!("macro_histogram").record(1.5);
            metrics::histogram!("macro_histogram").record(100.0);
        });

        // Both recordings reach the OTEL instrument and export as one series.
        let finished = export(&provider, &exporter);
        with_exported_metric(&finished, "macro_histogram", |metric| {
            let AggregatedMetrics::F64(MetricData::Histogram(hist)) = metric.data() else {
                panic!("expected an f64 histogram, got {:?}", metric.data());
            };
            let points: Vec<_> = hist.data_points().collect();
            assert_eq!(points.len(), 1);
            assert_eq!(points[0].count(), 2);
            assert_eq!(points[0].sum(), 101.5);
        });
    }

    #[test]
    fn macro_histogram_with_labels() {
        let (provider, exporter, meter) = exporting_meter();
        let recorder = OpenTelemetryRecorder::new(meter);
        metrics::with_local_recorder(&recorder, || {
            metrics::histogram!("macro_labeled_hist", "endpoint" => "/api").record(0.5);
            metrics::histogram!("macro_labeled_hist", "endpoint" => "/api").record(1.2);
        });

        // Recordings carry the label through to the exported data point.
        let finished = export(&provider, &exporter);
        with_exported_metric(&finished, "macro_labeled_hist", |metric| {
            let AggregatedMetrics::F64(MetricData::Histogram(hist)) = metric.data() else {
                panic!("expected an f64 histogram, got {:?}", metric.data());
            };
            let points: Vec<_> = hist.data_points().collect();
            assert_eq!(points.len(), 1);
            assert_eq!(points[0].count(), 2);
            assert_eq!(points[0].sum(), 1.7);
            let attrs: Vec<_> = points[0].attributes().collect();
            assert_eq!(attrs.len(), 1);
            assert_eq!(attrs[0].key.as_str(), "endpoint");
            assert_eq!(attrs[0].value.as_str(), "/api");
        });
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
        assert_eq!(
            recorder_gauge_value(&recorder, &Key::from_name("bpa.test.bundles")),
            1.0
        );
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
        assert_eq!(
            recorder_gauge_value(&recorder, &Key::from_name("undescribed_gauge")),
            1.0
        );
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

        assert_eq!(recorder_gauge_value(&recorder, &key_a), 1.0);
        assert_eq!(recorder_gauge_value(&recorder, &key_b), 10.0);
    }
}
