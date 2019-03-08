use std::time::{Duration, Instant};
use std::fmt;

pub trait Sampler {
  fn should_sample(&self, req: String) -> bool;
}

pub struct NeverSample;

impl Sampler for NeverSample {
  fn should_sample(&self, _req: String) -> bool {
    false
  }
}

pub struct AlwaysSample;

impl Sampler for AlwaysSample {
  fn should_sample(&self, _req: String) -> bool {
    true
  }
}

impl Clone for AlwaysSample {
   fn clone(&self) -> Self {
     AlwaysSample {}
   }
}

pub struct SpanContext {
  sampler: Box<AlwaysSample>
}

impl SpanContext {
  pub fn new() -> Self {
    SpanContext {
      sampler: Box::new(AlwaysSample{})
    }
  }

  pub fn span_from_request(&self, name: String, _req: String) -> Option<Span> {
    if (*self.sampler).should_sample(String::from("request_place_holder")) {
      Some(Span::new(name))
    }
    else {
      None
    }
  }
}

impl fmt::Debug for SpanContext {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
      write!(f, "SpanContext debug place holder")
    }
}

impl Clone for SpanContext {
   fn clone(&self) -> Self {
     SpanContext {
       sampler: Box::new((*self.sampler).clone())
     }
   }
}

#[derive(Clone)]
pub struct Span {
  name: String, 
  start_time: Option<Instant>, 
  duration: Option<Duration>
}

impl fmt::Debug for Span {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
      write!(f, "Hi: {}", 123)
    }
}

impl Span {
  pub fn new(name: String) -> Self {
    let mut res = Span {
      name,
      start_time: None,
      duration: None
    };
    res.start_span();
    res
  } 

  pub fn start_span(&mut self) {
    self.start_time = Some(Instant::now());
  }

  pub fn finish_span(&mut self) {
     if let Some(start) = self.start_time {
       let raw_duration = Instant::now().duration_since(start);
       self.duration = Some(raw_duration);
       println!("!!!!!!!!!!!!!!!!!!!!!!!!!!!!!! Closed span with duration {}", raw_duration.as_millis());
     }
  }
}
