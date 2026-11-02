_kiki_todo_enabled = True
_kiki_todo_len, _kiki_todo_type = len, type
_kiki_todo_str, _kiki_todo_list, _kiki_todo_dict, _kiki_todo_int = str, list, dict, int

class TodoList:
    def __init__(self):
        self._reset()

    def _check(self):
        if not _kiki_todo_enabled:
            raise RuntimeError("TODO list is disabled")

    def _validate(self, text, status):
        if _kiki_todo_type(text) != _kiki_todo_str or not text.strip() or _kiki_todo_len(text) > 512:
            raise ValueError("TODO text must be nonblank and at most 512 Unicode code points")
        if _kiki_todo_type(status) != _kiki_todo_str or status not in ("pending", "in_progress", "completed"):
            raise ValueError("TODO status must be pending, in_progress, or completed")

    def _index(self, item_id):
        if _kiki_todo_type(item_id) == _kiki_todo_int:
            for index in range(_kiki_todo_len(self._items)):
                if self._items[index]["id"] == item_id:
                    return index
        raise KeyError(f"{item_id}")

    def add(self, text, status="pending"):
        self._check()
        self._validate(text, status)
        if _kiki_todo_len(self._items) >= 64:
            raise ValueError("TODO list is limited to 64 items")
        item_id = self._next_id
        self._items.append({"id": item_id, "text": text, "status": status})
        self._next_id += 1
        return item_id

    def update(self, item_id, **fields):
        self._check()
        index = self._index(item_id)
        for key in fields:
            if key not in ("text", "status"):
                raise ValueError("Unexpected TODO field")
        old = self._items[index]
        text = fields.get("text", old["text"])
        status = fields.get("status", old["status"])
        self._validate(text, status)
        self._items[index] = {"id": old["id"], "text": text, "status": status}

    def remove(self, item_id):
        self._check()
        self._items.pop(self._index(item_id))

    def replace(self, items):
        self._check()
        if _kiki_todo_type(items) != _kiki_todo_list or _kiki_todo_len(items) > 64:
            raise ValueError("TODO replacement must be a list of at most 64 items")
        accepted = []
        for item in items:
            if _kiki_todo_type(item) != _kiki_todo_dict or "text" not in item or "status" not in item:
                raise ValueError("TODO replacement entries require text and status")
            for key in item:
                if key not in ("id", "text", "status"):
                    raise ValueError("Unexpected TODO field")
            self._validate(item["text"], item["status"])
            accepted.append({"id": self._next_id + _kiki_todo_len(accepted), "text": item["text"], "status": item["status"]})
        self._items = accepted
        self._next_id += _kiki_todo_len(accepted)
        return self.snapshot()

    def snapshot(self):
        self._check()
        return [{"id": item["id"], "text": item["text"], "status": item["status"]} for item in self._items]

    def _reset(self):
        self._items = []
        self._next_id = 1

todos = TodoList()
_kiki_todos = todos
