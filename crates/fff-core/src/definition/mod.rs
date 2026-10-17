mod languages;
mod parser;
mod rules;
mod scan;
mod suffix;

pub(crate) use languages::{Language, is_definition_match};

#[cfg(test)]
mod tests {
    use super::{Language, is_definition_match};

    #[test]
    fn generated_rules_classify_declared_names() {
        for (filename, line, expected) in [
            ("names.rs", "#[derive(Clone)] pub struct user_file;", true),
            ("names.rs", "impl user_file {}", false),
            ("names.py", "class user_file(Base):", true),
            ("names.py", "def other(value: user_file):", false),
            ("names.go", "type user_file struct {}", true),
            ("names.go", "func (s *user_file) other() {}", false),
            ("names.ts", "export class user_file extends Base {}", true),
            ("names.ts", "export const enum user_file { Ready }", true),
            ("names.ts", "const current: user_file = Mode.Ready;", false),
            ("names.ts", "function other(value: user_file) {}", false),
            ("names.lua", "local function user_file() end", true),
            ("names.lua", "function user_file.other() end", false),
            ("names.cpp", "struct user_file {", true),
            ("names.cpp", "struct Packet *user_file(void) {", true),
            ("names.cpp", "struct user_file *create(void) {", false),
            ("names.cpp", "struct user_file instance;", false),
            ("names.java", "public class user_file extends Base {", true),
            (
                "names.java",
                "public static <T> List<T> user_file(T... xs) {",
                true,
            ),
            (
                "names.java",
                "public static <user_file> List<user_file> create() {",
                false,
            ),
            ("names.java", "public user_file other() {", false),
            ("names.kt", "data class user_file(val x: Int)", true),
            ("names.kt", "fun user_file.other() {}", false),
            ("names.scala", "case class user_file(x: Int)", true),
            ("names.scala", "extension (value: user_file)", false),
            ("names.swift", "public struct user_file {", true),
            ("names.swift", "extension user_file {", false),
            ("names.rb", "def self.user_file?", true),
            ("names.rb", "def user_file.other", false),
            ("names.ex", "defp user_file?(arg) do", true),
            ("names.ex", "defimpl user_file, for: Other do", false),
            ("names.zig", "pub const user_file = struct {", true),
            ("names.zig", "fn other(value: user_file) void {", false),
            ("names.php", "#[Attribute] class user_file {", true),
            ("names.php", "class Other extends user_file {", false),
            ("names.sh", "user_file() {", true),
            ("names.sh", "echo user_file", false),
            ("names.ml", "type 'a user_file = 'a list", true),
            ("names.ml", "let other = user_file value", false),
            ("names.hs", "user_file :: Int -> Int", true),
        ] {
            let start = line.find("user_file").unwrap() as u32;
            let ranges = [(start, start + 9)];
            let language = Language::from_filename(filename) as u8;
            for fuzzy in [false, true] {
                assert_eq!(
                    is_definition_match(language, line.as_bytes(), &ranges, fuzzy),
                    expected,
                    "{filename}: {line}"
                );
                if expected {
                    for length in [255, 256, 257, 1024] {
                        let padded = format!("{line:<length$}");
                        assert_eq!(
                            is_definition_match(language, padded.as_bytes(), &ranges, fuzzy),
                            length <= 256,
                            "{filename}: {length} bytes, fuzzy={fuzzy}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn fuzzy_definition_ranges_stay_inside_the_name() {
        let language = Language::from_filename("file.rs") as u8;
        let line = b"fn user(file: u8) {}";
        assert!(is_definition_match(language, line, &[(3, 7)], true));
        assert!(!is_definition_match(
            language,
            line,
            &[(3, 7), (8, 12)],
            true
        ));
        assert!(!is_definition_match(language, line, &[], true));
        assert!(!is_definition_match(language, line, &[(3, 3)], true));
        assert!(!is_definition_match(255, line, &[(3, 7)], true));
    }

    #[test]
    fn generic_bounds_and_swift_class_properties_keep_the_declared_name() {
        for (filename, line, expected) in [
            (
                "x.ts",
                "const user_file = <T extends () => void>(callback: T) => callback;",
                true,
            ),
            (
                "x.ts",
                "user_file<T extends () => void>(callback: T) {}",
                true,
            ),
            (
                "x.ts",
                "const user_file = <T extends Array<() => void>>(callback: T) => callback;",
                true,
            ),
            (
                "x.ts",
                "other<T extends () => user_file>(callback: T) {}",
                false,
            ),
            (
                "x.kt",
                "fun <T : (Int) -> Unit> user_file(callback: T) {}",
                true,
            ),
            (
                "x.kt",
                "fun <T : (Int) -> user_file> other(callback: T) {}",
                false,
            ),
            ("x.swift", "public class var user_file: Int { 1 }", true),
            (
                "x.swift",
                "public class var other: user_file { value }",
                false,
            ),
            ("x.swift", "public class func user_file() {}", true),
            ("x.swift", "public class user_file {}", true),
        ] {
            let start = line.find("user_file").unwrap() as u32;
            for fuzzy in [false, true] {
                assert_eq!(
                    is_definition_match(
                        Language::from_filename(filename) as u8,
                        line.as_bytes(),
                        &[(start, start + 9)],
                        fuzzy,
                    ),
                    expected,
                    "{filename}: {line}",
                );
            }
        }
    }

    #[test]
    fn qualified_cpp_names_require_declaration_evidence() {
        for (line, name, expected) in [
            ("    std::sort(", "sort", false),
            ("    ns::Type::method(", "method", false),
            ("    ns::Type::method(items);", "method", false),
            ("void ns::Type::method(", "method", true),
            (
                "std::vector<int> ns::Type::method() const {",
                "method",
                true,
            ),
            ("Type::Type() {", "Type", true),
            ("Type::~Type() {", "Type", true),
            ("ns::Type::Type(", "Type", true),
        ] {
            let start = line.rfind(name).unwrap() as u32;
            let end = start + name.len() as u32;
            for fuzzy in [false, true] {
                assert_eq!(
                    is_definition_match(Language::C as u8, line.as_bytes(), &[(start, end)], fuzzy),
                    expected,
                    "{line}",
                );
            }
        }
    }
}
