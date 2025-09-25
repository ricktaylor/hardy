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
    counters: Arc<DashMap<String, OtelCounter<u64>>>,
    gauges: Arc<DashMap<String, OtelGauge<f64>>>,
    histograms: Arc<DashMap<String, OtelHistogram<f64>>>,
}

impl OpenTelemetryRecorder {
    /// Creates a new `OtelRecorder` that will create instruments using the provided
    /// OpenTelemetry `Meter`.
    pub fn new(meter: Meter) -> Self {
        OpenTelemetryRecorder {
            meter,
            counters: Arc::new(DashMap::new()),
            gauges: Arc::new(DashMap::new()),
            histograms: Arc::new(DashMap::new()),
        }
    }
}

impl Recorder for OpenTelemetryRecorder {
    fn describe_counter(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        // This is called by the `metrics` crate on the first use of a counter.
        // We eagerly create and cache the OTel counter here.
        let name = key.as_str().to_string();
        self.counters.entry(name.clone()).or_insert_with(|| {
            let mut counter = self.meter.u64_counter(name);
            if let Some(u) = unit {
                counter = counter.with_unit(u.as_str());
            }
            if !description.is_empty() {
                counter = counter.with_description(description.to_string());
            }
            counter.build()
        });
    }

    fn describe_gauge(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        let name = key.as_str().to_string();
        self.gauges.entry(name.clone()).or_insert_with(|| {
            let mut gauge = self.meter.f64_gauge(name);
            if let Some(u) = unit {
                gauge = gauge.with_unit(u.as_str());
            }
            if !description.is_empty() {
                gauge = gauge.with_description(description.to_string());
            }
            gauge.build()
        });
    }

    fn describe_histogram(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        let name = key.as_str().to_string();
        self.histograms.entry(name.clone()).or_insert_with(|| {
            let mut histogram = self.meter.f64_histogram(name);
            if let Some(u) = unit {
                histogram = histogram.with_unit(u.as_str());
            }
            if !description.is_empty() {
                histogram = histogram.with_description(description.to_string());
            }
            histogram.build()
        });
    }

    fn register_counter(&self, key: &Key, _metadata: &Metadata<'_>) -> Counter {
        let name = key.to_string();
        let counter = self
            .counters
            .entry(name.clone())
            .or_insert_with(|| self.meter.u64_counter(name).build())
            .value()
            .clone();
        let counter = InnerCounter {
            counter,
            labels: key
                .labels()
                .map(|label| KeyValue::new(label.key().to_string(), label.value().to_string()))
                .collect(),
        };
        Counter::from_arc(Arc::new(counter))
    }

    fn register_gauge(&self, key: &Key, _metadata: &Metadata<'_>) -> Gauge {
        let name = key.to_string();
        let gauge = self
            .gauges
            .entry(name.clone())
            .or_insert_with(|| self.meter.f64_gauge(name).build())
            .value()
            .clone();
        let gauge = InnerGauge {
            gauge,
            labels: key
                .labels()
                .map(|label| KeyValue::new(label.key().to_string(), label.value().to_string()))
                .collect(),
        };
        Gauge::from_arc(Arc::new(gauge))
    }

    fn register_histogram(&self, key: &Key, _metadata: &Metadata<'_>) -> Histogram {
        let name = key.to_string();
        let histogram = self
            .histograms
            .entry(name.clone())
            .or_insert_with(|| self.meter.f64_histogram(name).build())
            .value()
            .clone();
        let histogram = InnerHistogram {
            histogram,
            labels: key
                .labels()
                .map(|label| KeyValue::new(label.key().to_string(), label.value().to_string()))
                .collect(),
        };
        Histogram::from_arc(Arc::new(histogram))
    }
}

struct InnerCounter {
    counter: OtelCounter<u64>,
    labels: Vec<KeyValue>,
}

impl metrics::CounterFn for InnerCounter {
    fn increment(&self, value: u64) {
        self.counter.add(value, &self.labels);
    }

    fn absolute(&self, _value: u64) {
        panic!(
            "absolute() is not supported; OpenTelemetry counters are monotonic and can only be incremented."
        )
    }
}

struct InnerGauge {
    gauge: OtelGauge<f64>,
    labels: Vec<KeyValue>,
}

impl metrics::GaugeFn for InnerGauge {
    fn increment(&self, _value: f64) {
        panic!("Incrementing a gauge is not supported by this OpenTelemetry recorder.");
    }

    fn decrement(&self, _value: f64) {
        panic!("Decrementing a gauge is not supported by this OpenTelemetry recorder.");
    }

    fn set(&self, value: f64) {
        self.gauge.record(value, &self.labels)
    }
}

struct InnerHistogram {
    histogram: OtelHistogram<f64>,
    labels: Vec<KeyValue>,
}

impl metrics::HistogramFn for InnerHistogram {
    fn record(&self, value: f64) {
        self.histogram.record(value, &self.labels);
    }
}
