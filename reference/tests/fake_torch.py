"""A small numpy-backed stand-in for torch, so the GPU layer can be tested without a GPU."""
import numpy as np


class FT(np.ndarray):
    """A numpy array with the few torch-style methods the backend uses."""

    def to(self, arg):
        if isinstance(arg, str):          # a device name: nothing to do
            return self
        return np.asarray(self).astype(arg).view(FT)

    def sum(self, dim=None, **kw):
        return np.asarray(self).sum(axis=dim).view(FT)

    def cpu(self):
        return self

    def numpy(self):
        return np.asarray(self)

    def contiguous(self):
        return np.ascontiguousarray(np.asarray(self)).view(FT)

    def t(self):
        return np.asarray(self).T.view(FT)

    def data_ptr(self):
        return self.ctypes.data

    def copy_(self, other):
        np.copyto(np.asarray(self), np.asarray(other))
        return self


class FakeProps:
    total_memory = 16 * 2**30


class FakeCuda:
    @staticmethod
    def synchronize():
        pass

    @staticmethod
    def current_stream():
        class _Stream:
            cuda_stream = 0
        return _Stream()

    @staticmethod
    def max_memory_allocated():
        return 5 * 2**30

    @staticmethod
    def get_device_properties(i):
        return FakeProps()


class FakeTorch:
    int64, int32, int8, float32 = np.int64, np.int32, np.int8, np.float32
    cuda = FakeCuda

    @staticmethod
    def arange(*args, dtype=None, device=None):
        return np.arange(*args, dtype=dtype).view(FT)

    @staticmethod
    def stack(items, dim=0):
        return np.stack([np.asarray(i) for i in items], axis=dim).view(FT)

    @staticmethod
    def from_numpy(a):
        return np.asarray(a).view(FT)

    @staticmethod
    def zeros(shape, dtype=None, device=None):
        return np.zeros(shape, dtype=dtype).view(FT)

    @staticmethod
    def empty(shape, dtype=None, device=None):
        return np.zeros(shape, dtype=dtype).view(FT)

    @staticmethod
    def empty_like(a):
        return np.zeros_like(np.asarray(a)).view(FT)

    @staticmethod
    def full(shape, value, dtype=None, device=None):
        return np.full(shape, value, dtype=dtype).view(FT)

    @staticmethod
    def _int_mm(a, b):
        return (np.asarray(a).astype(np.int32) @ np.asarray(b).astype(np.int32)).view(FT)


class NoIntMm(FakeTorch):
    @staticmethod
    def _int_mm(a, b):
        raise RuntimeError("not supported in this build")
