use tree_sitter::Parser;

fn dump(name: &str, lang: tree_sitter::Language, src: &str) {
    let mut p = Parser::new();
    p.set_language(&lang).unwrap();
    let tree = p.parse(src, None).unwrap();
    println!("===== {} =====", name);
    println!("{}", tree.root_node().to_sexp());
}

fn main() {
    dump(
        "rust",
        tree_sitter_rust::LANGUAGE.into(),
        r#"
fn f(a: u32, b: u32) -> u32 {
    if a > 0 && b > 0 { return 1; }
    while a > 1 { for i in 0..3 { loop { break; } } }
    let x = match a { 0 => 1, 1 if b > 0 => 2, _ => 3 };
    let y = if a > 2 { 1 } else if b > 2 { 2 } else { 3 };
    let z: Result<u32, ()> = Ok(1);
    let w = z?;
    if let Some(q) = None::<u32> { }
    while let Some(q) = None::<u32> { }
    Some(1).map(|v| v + 1);
    x + y + w
}
impl S { fn m(&self, a: u32) -> u32 { a } fn n() {} }
"#,
    );
    dump(
        "python",
        tree_sitter_python::LANGUAGE.into(),
        r#"
def f(a, b=1, *args, **kwargs):
    if a and b:
        pass
    elif a or b:
        pass
    else:
        pass
    for i in range(3):
        while a:
            pass
    try:
        pass
    except ValueError:
        pass
    except Exception:
        pass
    finally:
        pass
    x = 1 if a else 2
    y = [i for i in range(3) if i]
    with open("f") as fh:
        pass
    match a:
        case 1:
            pass
        case _:
            pass
    assert a
    return x

class C:
    def m(self, a):
        return a

    async def am(self):
        pass

lam = lambda q: q
"#,
    );
    dump(
        "go",
        tree_sitter_go::LANGUAGE.into(),
        r#"
package main

func f(a int, b, c string) int {
	if a > 0 && b != "" {
	} else if a < 0 {
	} else {
	}
	for i := 0; i < 3; i++ {
	}
	for range []int{} {
	}
	switch a {
	case 1:
	case 2, 3:
	default:
	}
	switch v := any(a).(type) {
	case int:
	default:
	}
	select {
	case <-make(chan int):
	default:
	}
	if x, err := g(); err != nil {
	}
	return a
}

func (r *T) m(a int) {}
"#,
    );
    dump(
        "java",
        tree_sitter_java::LANGUAGE.into(),
        r#"
class C {
    int f(int a, String b) {
        if (a > 0 && b != null) { } else if (a < 0) { } else { }
        for (int i = 0; i < 3; i++) { }
        for (String s : new String[0]) { }
        while (a > 0) { }
        do { } while (a > 0);
        try { } catch (RuntimeException e) { } catch (Exception e) { } finally { }
        switch (a) {
            case 1: break;
            case 2: break;
            default: break;
        }
        switch (a) {
            case 1 -> { }
            default -> { }
        }
        int x = a > 0 ? 1 : 2;
        return x;
    }
    void g() {}
}
"#,
    );
    dump(
        "typescript",
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        r#"
function f(a: number, b: string = "x", ...rest: number[]): number {
  if (a > 0 && b !== "") { } else if (a < 0) { } else { }
  for (let i = 0; i < 3; i++) { }
  for (const k of []) { }
  for (const k in {}) { }
  while (a > 0) { }
  do { } while (a > 0);
  try { } catch (e) { } finally { }
  switch (a) {
    case 1: break;
    case 2: break;
    default: break;
  }
  const x = a > 0 ? 1 : 2;
  const y = a ?? 1;
  const z = a?.toString();
  const g = (p: number) => p + 1;
  return x;
}
class K { m(a: number) { return a; } get p() { return 1; } }
const arrow = async (q: number) => { return q; };
"#,
    );
    dump(
        "javascript",
        tree_sitter_javascript::LANGUAGE.into(),
        r#"
function f(a, b = 1, ...rest) {
  if (a && b) { } else { }
  const g = (p) => p;
  return a ? 1 : 2;
}
class K { m(a) { return a; } }
"#,
    );
}
