use opentelemetry::{
    propagation::{text_map_propagator::FieldIter, Extractor, Injector, TextMapPropagator},
    Context,
};
use opentelemetry_sdk::propagation::{BaggagePropagator, TraceContextPropagator};

#[derive(Copy, Clone, Eq, PartialEq)]
enum PropagationFormat {
    W3C,
    B3,
}

#[derive(Debug)]
pub struct OrderedPropagator {
    w3c: TraceContextPropagator,
    b3: opentelemetry_zipkin::Propagator,
    baggage: BaggagePropagator,
    fields: Vec<String>,
}

impl OrderedPropagator {
    pub fn new() -> Self {
        let w3c = TraceContextPropagator::new();
        let b3 = opentelemetry_zipkin::Propagator::new();
        let baggage = BaggagePropagator::new();

        Self {
            fields: w3c
                .fields()
                .chain(b3.fields())
                .chain(baggage.fields())
                .map(|s| s.to_string())
                .collect(),
            w3c,
            b3,
            baggage,
        }
    }
}

impl Default for OrderedPropagator {
    fn default() -> Self {
        Self::new()
    }
}

impl TextMapPropagator for OrderedPropagator {
    fn inject_context(&self, cx: &Context, injector: &mut dyn Injector) {
        match cx.get::<PropagationFormat>() {
            None => {}
            Some(PropagationFormat::W3C) => {
                self.w3c.inject_context(cx, injector);
            }
            Some(PropagationFormat::B3) => {
                self.b3.inject_context(cx, injector);
            }
        }
        self.baggage.inject_context(cx, injector);
    }

    fn extract_with_context(&self, cx: &Context, extractor: &dyn Extractor) -> Context {
        let cx = if self.w3c.fields().any(|f| extractor.get(f).is_some()) {
            self.w3c
                .extract_with_context(cx, extractor)
                .with_value(PropagationFormat::W3C)
        } else if self.b3.fields().any(|f| extractor.get(f).is_some()) {
            self.b3
                .extract_with_context(cx, extractor)
                .with_value(PropagationFormat::B3)
        } else {
            cx.clone()
        };
        self.baggage.extract_with_context(&cx, extractor)
    }

    fn fields(&self) -> FieldIter<'_> {
        FieldIter::new(self.fields.as_slice())
    }
}
