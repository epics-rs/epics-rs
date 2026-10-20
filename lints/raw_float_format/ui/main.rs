use std::fmt;
use std::rc::Rc;
use std::sync::Arc;

struct CFloat(f64);

impl fmt::Display for CFloat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0 as i64)
    }
}

struct Delegated(f64);

impl fmt::Display for Delegated {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

struct Path(f64, f32);

impl fmt::Display for Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)?;
        <f32 as fmt::LowerExp>::fmt(&self.1, f)
    }
}

fn render<T: fmt::Display>(items: &[T]) -> Vec<String> {
    items.iter().map(|v| v.to_string()).collect()
}

fn main() {
    let x = 1.5_f64;
    let y = 2.5_f32;
    let r = &x;
    let _ = format!("{}", x);
    let _ = format!("{x}");
    let _ = format!("{:?}", y);
    let _ = format!("{:e} {:E}", x, x);
    let _ = format!("{:.3}", r);
    println!("{}", x * 2.0);
    let _ = format!("{}", CFloat(x));
    let _ = format!("{} {:?}", 3_i32, Some(x));
    let _ = x.to_string();
    let _ = Delegated(x).to_string();
    let _ = render(&[x]);
    let _ = render(&[3_i32]);
    let _ = [y].iter().map(ToString::to_string).count();
    let _ = format!("{}", Box::new(x));
    let _ = format!("{} {}", Rc::new(x), Arc::new(&y));
    let _ = Path(x, y).to_string();
    let _ = Box::new(x).to_string();
    let _ = Arc::new(y).to_string();
}
