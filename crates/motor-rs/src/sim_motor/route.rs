//! One routed axis of motorSim's `route.c`: the time-optimal four-phase path
//! (accelerate at Amax, coast at most at Vmax, decelerate, coast at the final
//! velocity) from the last demand to an endpoint. motorSim routes one axis
//! with `Tsync = Tcoast = 0`, so only that case is kept.

/// `route_demand_t` for one axis.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Demand {
    pub t: f64,
    pub p: f64,
    pub v: f64,
}

/// `route_reroute_t` as motorSim uses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Reroute {
    /// `ROUTE_CALC_ROUTE`: keep the path while the endpoint is unchanged.
    Calc,
    /// `ROUTE_NEW_ROUTE`: recalculate from the current demand.
    New,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    BadParam,
    NegTime,
}

/// `route_unknown_t`.
const V2: u8 = 1;
const T: u8 = 2;
const T2: u8 = 4;
const T4: u8 = 8;

/// `IS_ZERO(a, scale)` (route.c:39).
fn is_zero(a: f64, scale: f64) -> bool {
    a.abs() <= (2.0 * f64::EPSILON * scale).abs()
}

/// `path_t`.
#[derive(Debug, Clone, Copy, Default)]
struct Path {
    dist: f64,
    vi: f64,
    vf: f64,
    v2: f64,
    t1: f64,
    t2: f64,
    t3: f64,
    t4: f64,
    t: f64,
}

impl Path {
    /// `routeDemand` (route.c:73): position and velocity `t` from the end
    /// of the path (`t <= 0` on it), the position relative to the endpoint.
    fn demand(&self, mut t: f64) -> (f64, f64) {
        let accel1 = if self.t1 != 0.0 {
            (self.v2 - self.vi) / self.t1
        } else {
            0.0
        };
        let accel2 = if self.t3 != 0.0 {
            (self.vf - self.v2) / self.t3
        } else {
            0.0
        };
        if t >= -self.t4 {
            return (self.vf * t, self.vf);
        }
        t += self.t4;
        let mut p = -self.vf * self.t4;
        if t >= -self.t3 {
            let v = self.vf + accel2 * t;
            return (p + 0.5 * (v + self.vf) * t, v);
        }
        t += self.t3;
        p -= 0.5 * (self.v2 + self.vf) * self.t3;
        if t >= -self.t2 {
            return (p + self.v2 * t, self.v2);
        }
        t += self.t2;
        p -= self.v2 * self.t2;
        if t >= -self.t1 {
            let v = self.v2 + accel1 * t;
            return (p + 0.5 * (v + self.v2) * t, v);
        }
        // route.c:121-122, t2 where t1 is meant included.
        (
            p + 0.5 * (self.vi + self.v2) * self.t2 + (t + self.t1) * self.vi,
            self.vi,
        )
    }

    /// `routeFindWhichV2Sqrt` (route.c:197).
    fn which_v2_sqrt(&mut self, ai: f64, lin_term: f64, sqrt_term: f64, unknown: u8) {
        let sqrt_term = sqrt_term.max(0.0).sqrt();
        let minus = if unknown == T2 { ai > 0.0 } else { ai <= 0.0 };
        self.v2 = if minus {
            lin_term - sqrt_term
        } else {
            lin_term + sqrt_term
        };
    }

    /// `routeFindPath` (route.c:302).
    fn find(&mut self, accel: f64, unknowns: u8) -> Status {
        if accel <= 0.0 {
            return Status::BadParam;
        }
        let mut status = Status::Ok;
        match unknowns {
            u if u == V2 | T || u == V2 | T4 || u == V2 | T2 => {
                let min_accel_time = ((self.vi - self.vf) / accel).abs();
                let mut max_t2_time = self.t2;
                let other = unknowns & !V2;
                let min_not_t2_dist = match other {
                    T => 0.5 * (self.vi + self.vf) * min_accel_time + self.vf * self.t4,
                    T4 => {
                        0.5 * (self.vi + self.vf) * min_accel_time
                            + self.vf * (self.t - self.t2 - min_accel_time)
                    }
                    _ => {
                        max_t2_time = self.t - self.t4 - min_accel_time;
                        0.5 * (self.vi + self.vf) * min_accel_time + self.vf * self.t4
                    }
                };
                let vi_dist = min_not_t2_dist + self.vi * max_t2_time;
                let vf_dist = min_not_t2_dist + self.vf * max_t2_time;
                if self.dist == vi_dist {
                    self.v2 = self.vi;
                } else if self.dist == vf_dist {
                    self.v2 = self.vf;
                } else if (self.dist < vi_dist && self.dist > vf_dist)
                    || (self.dist > vi_dist && self.dist < vf_dist)
                {
                    let ai = if self.vf > self.vi { accel } else { -accel };
                    self.v2 = match other {
                        T4 => {
                            (self.dist + 0.5 * (self.vi - self.vf) * (self.vi - self.vf) / ai
                                - self.vf * (self.t - self.t2))
                                / max_t2_time
                        }
                        _ => {
                            (self.dist + 0.5 * (self.vi * self.vi - self.vf * self.vf) / ai
                                - self.vf * self.t4)
                                / max_t2_time
                        }
                    };
                } else {
                    let ai = if self.dist < vi_dist && self.dist < vf_dist {
                        -accel
                    } else {
                        accel
                    };
                    let (lin_term, sqrt_term) = match other {
                        T => {
                            let h = 0.5 * self.t2 * ai;
                            (
                                -0.5 * ai * self.t2,
                                h * h
                                    + 0.5 * (self.vi * self.vi + self.vf * self.vf)
                                    + ai * (self.dist - self.vf * self.t4),
                            )
                        }
                        T4 => {
                            let h = 0.5 * ai * self.t2;
                            (
                                self.vf - 0.5 * ai * self.t2,
                                h * h
                                    + 0.5 * (self.vi - self.vf) * (self.vi - self.vf)
                                    + ai * (self.dist - self.vf * self.t),
                            )
                        }
                        _ => {
                            let lin = 0.5 * (ai * (self.t - self.t4) + self.vi + self.vf);
                            (
                                lin,
                                lin * lin
                                    - 0.5 * (self.vi * self.vi + self.vf * self.vf)
                                    - ai * (self.dist - self.vf * self.t4),
                            )
                        }
                    };
                    self.which_v2_sqrt(ai, lin_term, sqrt_term, other);
                }
                self.t1 = ((self.v2 - self.vi) / accel).abs();
                self.t3 = ((self.vf - self.v2) / accel).abs();
                if unknowns & T != 0 {
                    self.t = self.t1 + self.t2 + self.t3 + self.t4;
                }
                if unknowns & T4 != 0 {
                    self.t4 = self.t - (self.t1 + self.t2 + self.t3);
                }
                if unknowns & T2 != 0 {
                    self.t2 = self.t - (self.t1 + self.t3 + self.t4);
                }
            }
            u if u == T | T4 || u == T | T2 || u == T2 | T4 => {
                self.t1 = ((self.v2 - self.vi) / accel).abs();
                self.t3 = ((self.vf - self.v2) / accel).abs();
                let dist = self.dist
                    - 0.5 * ((self.vi + self.v2) * self.t1 + (self.v2 + self.vf) * self.t3);
                if unknowns & T != 0 {
                    if unknowns & T4 != 0 && self.vf != 0.0 {
                        self.t4 = (dist - self.v2 * self.t2) / self.vf;
                    } else if unknowns & T2 != 0 && self.v2 != 0.0 {
                        self.t2 = (dist - self.vf * self.t4) / self.v2;
                    } else {
                        status = Status::BadParam;
                    }
                    self.t = self.t1 + self.t2 + self.t3 + self.t4;
                } else if self.v2 != self.vf {
                    self.t2 = (dist - self.vf * (self.t - self.t1 - self.t3)) / (self.v2 - self.vf);
                    self.t4 = self.t - self.t1 - self.t2 - self.t3;
                } else {
                    status = Status::BadParam;
                }
            }
            _ => status = Status::BadParam,
        }

        let negative = |p: &Path| p.t1 < 0.0 || p.t2 < 0.0 || p.t3 < 0.0 || p.t4 < 0.0 || p.t < 0.0;
        if status == Status::Ok && negative(self) {
            let scale = self.t;
            for x in [&mut self.t1, &mut self.t2, &mut self.t3, &mut self.t4] {
                if is_zero(*x, scale) {
                    *x = 0.0;
                }
            }
            if negative(self) {
                status = Status::NegTime;
                for x in [
                    &mut self.t1,
                    &mut self.t2,
                    &mut self.t3,
                    &mut self.t4,
                    &mut self.t,
                ] {
                    if *x < 0.0 {
                        *x = 0.0;
                    }
                }
            }
        }
        if status == Status::Ok
            && !is_zero(self.t - (self.t1 + self.t2 + self.t3 + self.t4), self.t)
        {
            status = Status::NegTime;
        }
        status
    }

    /// `routeFindPathWithVmax` (route.c:561).
    fn find_with_vmax(&mut self, amax: f64, vmax: f64, unknown: u8) -> Status {
        self.t2 = 0.0;
        let mut status = self.find(amax, V2 | unknown);
        if status == Status::Ok && self.v2.abs() > vmax {
            self.v2 = if self.v2 >= 0.0 { vmax } else { -vmax };
            status = self.find(amax, T2 | unknown);
        }
        status
    }
}

/// `route_t` for one axis with `Tsync = Tcoast = 0`.
#[derive(Debug, Clone)]
pub(super) struct Route {
    amax: f64,
    vmax: f64,
    demand: Demand,
    path: Path,
    endp: Demand,
}

impl Route {
    /// `routeNew` (route.c:617).
    pub fn new(demand: Demand, amax: f64, vmax: f64) -> Self {
        Self {
            amax,
            vmax,
            demand,
            path: Path {
                vi: demand.v,
                v2: demand.v,
                vf: demand.v,
                ..Path::default()
            },
            endp: demand,
        }
    }

    /// `routeSetDemand` (route.c:916).
    pub fn set_demand(&mut self, demand: Demand) {
        self.demand = demand;
    }

    /// `routeGetParams` (route.c:1013).
    pub fn params(&self) -> (f64, f64) {
        (self.amax, self.vmax)
    }

    /// `routeSetParams` (route.c:954): refused, keeping the old ones, unless
    /// both are positive and Vmax exceeds the demand's speed.
    pub fn set_params(&mut self, amax: f64, vmax: f64) {
        if amax > 0.0 && vmax > 0.0 && vmax > self.demand.v.abs() {
            self.amax = amax;
            self.vmax = vmax;
        }
    }

    /// `routeFind` (route.c:704): the demand at `nextp.t` on a path to
    /// `endp`, whose time it sets when the path is recalculated.
    pub fn find(&mut self, reroute: Reroute, endp: &mut Demand, nextp: &mut Demand) {
        let old_path_ok = reroute == Reroute::Calc
            && is_zero(endp.t - self.endp.t, endp.t)
            && is_zero(endp.p - self.endp.p, 40.0)
            && (endp.v - self.endp.v).abs() < self.vmax * 1.0e-10;
        if !old_path_ok {
            let path = &mut self.path;
            path.dist = endp.p - self.demand.p;
            path.vi = self.demand.v;
            path.vf = endp.v;
            path.t2 = 0.0;
            path.t4 = 0.0;
            path.t = endp.t - self.demand.t;
            let short_path = reroute != Reroute::New && nextp.t >= endp.t;
            let status = if short_path {
                path.find_with_vmax(self.amax, self.vmax, T4)
            } else {
                Status::BadParam
            };
            if !short_path || status != Status::Ok {
                path.t4 = 0.0;
                if path.find_with_vmax(self.amax, self.vmax, T) == Status::BadParam {
                    return;
                }
                endp.t = self.demand.t + path.t;
            }
        }
        let (p, v) = self.path.demand(nextp.t - endp.t);
        nextp.p = p + endp.p;
        nextp.v = v;
        self.demand = *nextp;
        self.endp = *endp;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step_until_stopped(route: &mut Route, endp: &mut Demand, next: &mut Demand) -> usize {
        let mut reroute = Reroute::New;
        for tick in 1..1000 {
            next.t += 0.1;
            route.find(reroute, endp, next);
            reroute = Reroute::Calc;
            if next.v == 0.0 {
                return tick;
            }
        }
        panic!("never stopped");
    }

    /// A unit move at Vmax 1, Amax 5 is the 1.2 s trapezoid: 0.2 s ramps
    /// either side of a 0.8 s coast.
    #[test]
    fn a_move_takes_the_trapezoid_time() {
        let start = Demand {
            t: 0.0,
            p: 0.0,
            v: 0.0,
        };
        let mut route = Route::new(start, 5.0, 1.0);
        let mut endp = Demand { p: 1.0, ..start };
        let mut next = start;
        let ticks = step_until_stopped(&mut route, &mut endp, &mut next);
        assert_eq!(ticks, 12);
        assert!((next.p - 1.0).abs() < 1e-12, "{next:?}");
        assert!((endp.t - 1.2).abs() < 1e-12, "{endp:?}");
    }

    /// A move too short to reach Vmax is a triangle.
    #[test]
    fn a_short_move_never_reaches_vmax() {
        let start = Demand {
            t: 0.0,
            p: 0.0,
            v: 0.0,
        };
        let mut route = Route::new(start, 1.0, 10.0);
        let mut endp = Demand { p: 1.0, ..start };
        let mut next = start;
        next.t += 0.1;
        route.find(Reroute::New, &mut endp, &mut next);
        assert!((endp.t - 2.0).abs() < 1e-12, "{endp:?}");
    }
}
