class MyClass:
    pass

class Vector:
    def __add__(self, other) -> MyClass:
        pass

    def __and__(self, other) -> MyClass:
        pass

added = Vector() + Vector()
added

intersected = Vector() & Vector()
intersected
