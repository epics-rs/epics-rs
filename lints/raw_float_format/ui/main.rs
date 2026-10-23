use std::collections::BTreeMap;
use std::fmt;
use std::marker::PhantomData;
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

fn show<T: fmt::Debug>(v: T) -> String {
    format!("{v:?}")
}

#[derive(Debug)]
struct Wrapper<T>(T);

struct HandWritten(f64, u32);

impl fmt::Debug for HandWritten {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("HandWritten")
            .field(&self.0)
            .field(&self.1)
            .finish()
    }
}

struct Written(f64);

impl fmt::Debug for Written {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Written({:?})", self.0)
    }
}

#[derive(Debug)]
struct Reading {
    value: f64,
    count: u32,
}

#[derive(Debug)]
struct Count(u32);

#[derive(Debug)]
struct Chain {
    next: Option<Box<Chain>>,
    n: i32,
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
    let _ = format!("{:?}", vec![x]);
    let _ = format!("{:?}", (1_u8, y));
    let _ = format!("{:?}", [x; 2]);
    let _ = format!("{:?}", &[r][..]);
    let _ = format!("{:?}", Some(&r));
    let _ = format!("{:?}", BTreeMap::from([(1_u8, x)]));
    let _ = format!("{:?}", Wrapper(y));
    let _ = format!("{:?}", Reading { value: x, count: 1 });
    let _ = format!("{:?}", Some(Wrapper(x)));
    let _ = format!("{:?}", vec![HandWritten(x, 1)]);
    let _ = format!("{:?}", Written(x));
    let _ = format!("{:?}", Some(Count(1)));
    let _ = format!("{:?}", Chain { next: None, n: 1 });
    let _ = format!("{:?}", PhantomData::<f64>);
    let _ = format!("{:?}", Rc::downgrade(&Rc::new(x)));
    let _ = show(vec![y]);
    Ok::<(), Vec<f64>>(()).unwrap();
    let _: &dyn fmt::Display = &x;
    let _: Box<dyn fmt::Debug> = Box::new(Some(y));
    let _: &dyn fmt::Debug = &3_i32;
}
