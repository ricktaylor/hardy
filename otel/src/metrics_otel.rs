use super::*;
use dashmap::DashMap;
use metrics::{Counter, Gauge, Histogram, Key, KeyName, Metadata, Recorder, SharedString, Unit};
use opentelemetry::metrics::{
    Counter as OtelCounter, Gauge as OtelGauge, Histogram as OtelHistogram, Meter,
};
use std::sync::Arc;

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
                    let mut counter = self.meter.u64_counter(key.name().to_string());
                    if let Some(desc) = self.counter_descs.get(key.name()) {
                        let (unit, description) = desc.value();
                        if let Some(u) = unit {
                            counter = counter.with_unit(otel_unit(u));
                        }
                        if !description.is_empty() {
                            counter = counter.with_description(description.to_string());
                        }
                    }
                    Arc::new(InnerCounter {
                        counter: counter.build(),
                        labels: key
                            .labels()
                            .map(|label| {
                                KeyValue::new(label.key().to_string(), label.value().to_string())
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
                    let mut gauge = self.meter.f64_gauge(key.name().to_string());
                    if let Some(desc) = self.gauge_descs.get(key.name()) {
                        let (unit, description) = desc.value();
                        if let Some(u) = unit {
                            gauge = gauge.with_unit(otel_unit(u));
                        }
                        if !description.is_empty() {
                            gauge = gauge.with_description(description.to_string());
                        }
                    }
                    Arc::new(InnerGauge {
                        gauge: gauge.build(),
                        labels: key
                            .labels()
                            .map(|label| {
                                KeyValue::new(label.key().to_string(), label.value().to_string())
                            })
                            .collect(),
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
                    let mut histogram = self.meter.f64_histogram(key.name().to_string());
                    if let Some(desc) = self.histogram_descs.get(key.name()) {
                        let (unit, description) = desc.value();
                        if let Some(u) = unit {
                            histogram = histogram.with_unit(otel_unit(u));
                        }
                        if !description.is_empty() {
                            histogram = histogram.with_description(description.to_string());
                        }
                    }
                    Arc::new(InnerHistogram {
                        histogram: histogram.build(),
                        labels: key
                            .labels()
                            .map(|label| {
                                KeyValue::new(label.key().to_string(), label.value().to_string())
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
}

impl metrics::GaugeFn for InnerGauge {
    fn increment(&self, _value: f64) {
        unimplemented!("Incrementing a gauge is not supported by this OpenTelemetry recorder")
    }

    fn decrement(&self, _value: f64) {
        unimplemented!("Decrementing a gauge is not supported by this OpenTelemetry recorder")
    }

    fn set(&self, value: f64) {
        self.gauge.record(value, &self.labels)
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
