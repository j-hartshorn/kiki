(comment) @comment
(string) @string

[
  (integer)
  (float)
  (none)
  (true)
  (false)
] @number

(decorator) @function
(function_definition
  name: (identifier) @function)
(class_definition
  name: (identifier) @type)

((call
  function: (identifier) @type)
 (#match?
   @type
   "^(abs|all|any|ascii|bin|bool|breakpoint|bytearray|bytes|callable|chr|classmethod|compile|complex|delattr|dict|dir|divmod|enumerate|eval|exec|filter|float|format|frozenset|getattr|globals|hasattr|hash|help|hex|id|input|int|isinstance|issubclass|iter|len|list|locals|map|max|memoryview|min|next|object|oct|open|ord|pow|print|property|range|repr|reversed|round|set|setattr|slice|sorted|staticmethod|str|sum|super|tuple|type|vars|zip|__import__)$"))

[
  "and"
  "as"
  "assert"
  "async"
  "await"
  "break"
  "case"
  "class"
  "continue"
  "def"
  "del"
  "elif"
  "else"
  "except"
  "finally"
  "for"
  "from"
  "global"
  "if"
  "import"
  "in"
  "is"
  "is not"
  "lambda"
  "match"
  "nonlocal"
  "not"
  "not in"
  "or"
  "pass"
  "raise"
  "return"
  "try"
  "while"
  "with"
  "yield"
] @keyword
