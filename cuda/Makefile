NVCC ?= $(shell command -v nvcc 2>/dev/null || echo /usr/local/cuda/bin/nvcc)
CUDA_ARCH ?=
TARGET := libcfmm_gpu.so
SRC := cfmm_solver.cu
HDR := cfmm_solver.h

NVCC_FLAGS := -O3 -arch=$(CUDA_ARCH) --ptxas-options=-v -Xcompiler -fPIC,-Wall,-Wextra -shared -std=c++17

.PHONY: all clean

all: $(TARGET)

$(TARGET): $(SRC) $(HDR)
ifeq ($(strip $(CUDA_ARCH)),)
	$(error CUDA_ARCH is required. Set it for your GPU, e.g.: make -C cuda CUDA_ARCH=sm_89)
endif
	$(NVCC) $(NVCC_FLAGS) -o $@ $(SRC)

clean:
	rm -f $(TARGET)
