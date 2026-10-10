//! Tiny expression language for wavetable formulas.
//!
//! Variables: `x` (phase, -1..1 across the cycle), `y` (table position, 0..1 across frames),
//! `w` (phase 0..1), `n` (frame index). Constants `pi`, `e`.
//! Operators: `+ - * / % ^`, unary `-`, comparisons `< > <= >=` (1 or 0), parentheses.
//! Functions: sin cos tan asin acos atan tanh abs sqrt exp log log10 floor ceil round sign
//! frac, min(a,b) max(a,b) pow(a,b) clamp(v,lo,hi) saw(t) square(t) tri(t) (period 1).

use crate::Error;

#[derive(Debug, Clone)]
enum Node {
    Num(f64),
    Var(usize),
    Neg(Box<Node>),
    Bin(char, Box<Node>, Box<Node>),
    Cmp(&'static str, Box<Node>, Box<Node>),
    Call(&'static str, Vec<Node>),
}

/// Variable values passed to [`Expr::eval`], in this order.
pub const VARS: [&str; 4] = ["x", "y", "w", "n"];

#[derive(Debug, Clone)]
pub struct Expr(Node);

const FUNCS: [(&str, usize); 22] = [
    ("sin", 1),
    ("cos", 1),
    ("tan", 1),
    ("asin", 1),
    ("acos", 1),
    ("atan", 1),
    ("tanh", 1),
    ("abs", 1),
    ("sqrt", 1),
    ("exp", 1),
    ("log", 1),
    ("log10", 1),
    ("floor", 1),
    ("ceil", 1),
    ("round", 1),
    ("sign", 1),
    ("frac", 1),
    ("saw", 1),
    ("square", 1),
    ("tri", 1),
    ("min", 2),
    ("max", 2),
];
const FUNCS_N: [(&str, usize); 2] = [("pow", 2), ("clamp", 3)];

impl Expr {
    pub fn parse(src: &str) -> Result<Self, Error> {
        let tokens = lex(src)?;
        let mut p = Parser { t: &tokens, i: 0 };
        let node = p.cmp()?;
        if p.i != tokens.len() {
            return Err(Error::Expr(format!("unexpected `{}`", tokens[p.i])));
        }
        Ok(Expr(node))
    }

    pub fn eval(&self, vars: &[f64; 4]) -> f64 {
        eval(&self.0, vars)
    }
}

fn lex(src: &str) -> Result<Vec<String>, Error> {
    let mut out = Vec::new();
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c.is_ascii_digit() || c == '.' {
            let s = i;
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                i += 1;
            }
            out.push(chars[s..i].iter().collect());
        } else if c.is_ascii_alphabetic() || c == '_' {
            let s = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            out.push(chars[s..i].iter().collect::<String>().to_ascii_lowercase());
        } else if (c == '<' || c == '>') && chars.get(i + 1) == Some(&'=') {
            out.push(format!("{c}="));
            i += 2;
        } else if "+-*/%^(),<>".contains(c) {
            out.push(c.to_string());
            i += 1;
        } else {
            return Err(Error::Expr(format!("unexpected character `{c}`")));
        }
    }
    Ok(out)
}

struct Parser<'a> {
    t: &'a [String],
    i: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&str> {
        self.t.get(self.i).map(String::as_str)
    }

    fn eat(&mut self, s: &str) -> bool {
        if self.peek() == Some(s) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn cmp(&mut self) -> Result<Node, Error> {
        let lhs = self.sum()?;
        for op in ["<=", ">=", "<", ">"] {
            if self.eat(op) {
                return Ok(Node::Cmp(op, Box::new(lhs), Box::new(self.sum()?)));
            }
        }
        Ok(lhs)
    }

    fn sum(&mut self) -> Result<Node, Error> {
        let mut lhs = self.product()?;
        loop {
            let op = match self.peek() {
                Some("+") => '+',
                Some("-") => '-',
                _ => return Ok(lhs),
            };
            self.i += 1;
            lhs = Node::Bin(op, Box::new(lhs), Box::new(self.product()?));
        }
    }

    fn product(&mut self) -> Result<Node, Error> {
        let mut lhs = self.unary()?;
        loop {
            let op = match self.peek() {
                Some("*") => '*',
                Some("/") => '/',
                Some("%") => '%',
                _ => return Ok(lhs),
            };
            self.i += 1;
            lhs = Node::Bin(op, Box::new(lhs), Box::new(self.unary()?));
        }
    }

    fn unary(&mut self) -> Result<Node, Error> {
        if self.eat("-") {
            return Ok(Node::Neg(Box::new(self.unary()?)));
        }
        if self.eat("+") {
            return self.unary();
        }
        self.power()
    }

    fn power(&mut self) -> Result<Node, Error> {
        let base = self.atom()?;
        if self.eat("^") {
            // right-associative, binds tighter than unary minus on the left only
            return Ok(Node::Bin('^', Box::new(base), Box::new(self.unary()?)));
        }
        Ok(base)
    }

    fn atom(&mut self) -> Result<Node, Error> {
        let tok = self.peek().ok_or_else(|| Error::Expr("unexpected end of formula".into()))?.to_owned();
        self.i += 1;
        if tok == "(" {
            let e = self.cmp()?;
            if !self.eat(")") {
                return Err(Error::Expr("missing `)`".into()));
            }
            return Ok(e);
        }
        if let Ok(v) = tok.parse::<f64>() {
            return Ok(Node::Num(v));
        }
        match tok.as_str() {
            "pi" => return Ok(Node::Num(std::f64::consts::PI)),
            "e" => return Ok(Node::Num(std::f64::consts::E)),
            _ => {}
        }
        if let Some(idx) = VARS.iter().position(|v| *v == tok) {
            return Ok(Node::Var(idx));
        }
        let (name, arity) = FUNCS
            .iter()
            .chain(FUNCS_N.iter())
            .find(|(n, _)| *n == tok)
            .copied()
            .ok_or_else(|| Error::Expr(format!("unknown name `{tok}`")))?;
        if !self.eat("(") {
            return Err(Error::Expr(format!("`{name}` needs `(`")));
        }
        let mut args = vec![self.cmp()?];
        while self.eat(",") {
            args.push(self.cmp()?);
        }
        if !self.eat(")") {
            return Err(Error::Expr(format!("missing `)` after {name} arguments")));
        }
        if args.len() != arity {
            return Err(Error::Expr(format!("`{name}` takes {arity} argument(s), got {}", args.len())));
        }
        Ok(Node::Call(name, args))
    }
}

fn eval(n: &Node, v: &[f64; 4]) -> f64 {
    match n {
        Node::Num(x) => *x,
        Node::Var(i) => v[*i],
        Node::Neg(a) => -eval(a, v),
        Node::Bin(op, a, b) => {
            let (a, b) = (eval(a, v), eval(b, v));
            match op {
                '+' => a + b,
                '-' => a - b,
                '*' => a * b,
                '/' => a / b,
                '%' => a.rem_euclid(b),
                _ => a.powf(b),
            }
        }
        Node::Cmp(op, a, b) => {
            let (a, b) = (eval(a, v), eval(b, v));
            let r = match *op {
                "<" => a < b,
                ">" => a > b,
                "<=" => a <= b,
                _ => a >= b,
            };
            r as u8 as f64
        }
        Node::Call(f, args) => {
            let a: Vec<f64> = args.iter().map(|x| eval(x, v)).collect();
            let t = a[0];
            match *f {
                "sin" => t.sin(),
                "cos" => t.cos(),
                "tan" => t.tan(),
                "asin" => t.asin(),
                "acos" => t.acos(),
                "atan" => t.atan(),
                "tanh" => t.tanh(),
                "abs" => t.abs(),
                "sqrt" => t.sqrt(),
                "exp" => t.exp(),
                "log" => t.ln(),
                "log10" => t.log10(),
                "floor" => t.floor(),
                "ceil" => t.ceil(),
                "round" => t.round(),
                "sign" => t.signum(),
                "frac" => t - t.floor(),
                "saw" => 2.0 * (t - t.floor()) - 1.0,
                "square" => if t - t.floor() < 0.5 { 1.0 } else { -1.0 },
                "tri" => 1.0 - 4.0 * ((t - t.floor()) - 0.5).abs(),
                "min" => t.min(a[1]),
                "max" => t.max(a[1]),
                "pow" => t.powf(a[1]),
                _ => t.clamp(a[1].min(a[2]), a[1].max(a[2])), // clamp
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(s: &str, x: f64, y: f64) -> f64 {
        Expr::parse(s).unwrap().eval(&[x, y, (x + 1.0) / 2.0, 0.0])
    }

    #[test]
    fn arithmetic_and_precedence() {
        assert_eq!(ev("1 + 2 * 3", 0.0, 0.0), 7.0);
        assert_eq!(ev("-2 ^ 2", 0.0, 0.0), -4.0);
        assert_eq!(ev("2 ^ 3 ^ 2", 0.0, 0.0), 512.0);
        assert_eq!(ev("(x < 0) * 2", -0.5, 0.0), 2.0);
        assert!((ev("sin(x * pi)", 0.5, 0.0) - 1.0).abs() < 1e-12);
        assert_eq!(ev("clamp(x * 3, -1, 1)", 0.9, 0.0), 1.0);
        assert_eq!(ev("max(x, y)", 0.2, 0.7), 0.7);
    }

    #[test]
    fn errors_are_reported() {
        for bad in ["sin x", "1 +", "foo(1)", "max(1)", "(1", "1 $ 2"] {
            assert!(Expr::parse(bad).is_err(), "{bad}");
        }
    }
}
