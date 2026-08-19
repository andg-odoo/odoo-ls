class Spam:
    def eggs(self, a, b=1):
        """Return the first argument."""
        return a

def foo(a, b=1):
    return a

def with_varargs(a, *rest):
    return a

foo()
foo(1, 2)
foo(b=2)
Spam().eggs(1)
with_varargs(1, 2, 3)
