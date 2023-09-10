# kZero

A from-scratch implementation of the [AlphaGo](https://www.nature.com/articles/nature24270), [AlphaZero](https://arxiv.org/abs/1712.01815) and [MuZero](https://www.nature.com/articles/s41586-020-03051-4.epdf) papers for different board games, written in a combination of Python and Rust.

This repository also includes a fairly involved neural network inference framework that can run ONNX files either on the CPU or on GPUs using cuda/cudnn/cublas/nvrtc, see the relevant chapter below for more details.

## Project Overview

### Top level

The top-level overview is shown in the figure above. The neural network training is implemented in Python, and Rust is used to implement the more performance-critical selfplay. The two parts communicate though a TCP connection and the file system.

![Top level system diagram](./docs/arch_overview.svg)

During a training loop the training framework connects to the selfplay TCP server running on localhost and gives it some initial settings. The selfplay server writes generated games to a file, and when it has generated enough games it signals this to the training framework. The network is then trained on these new games, and when training finishes the new network is sent to the selfplay server, closing the loop.

### Selfplay server (Rust)

The selfplay server is shown in the figure below. The orange boxes are the different threads running in the system.

![Selfplay server diagram](./docs/arch_selfplay.svg)

* The _commander_ receives commands from the TCP socket and forwards them to the appropriate place. Selfplay settings and hyperparameters are sent to the executors, and new neural networks are loaded from the filesystem and sent to the executors.
* The _generator_ thread pools run multiple simulations concurrently. Each simulation has its own MCTS tree that is grown over time. NN evaluation requests are sent to an executor.
* The _executor_ threads collect NN evaluations requests into batches and use the NN inference framework described later to run these evaluations, sending results back to the corresponding generator.
* Finally the _collector_ receives finished simulations from the generators, writes them to a file and notifies the TCP socket once enough simulations have finished.

Much effort was put into optimizing this system:

* Rust is used to get good baseline performance for the tricky code in tree search, memory transfers, thread interactions, NN input encoding, ...
* The MCTS itself uses virtual loss to collect small batches of evaluations.
* Many simulations are run concurrently on async thread pools. This allows for a second level of batching in the executors.
* Each GPU can have multiple executor threads, enabling concurrent batching, memory transfers and execution.
* Multiple GPUs can be used at full speed.

### Training (Python)

Finally the training component is shown in the figure below.

![Training diagram](./docs/arch_training.svg)

The python framework manages the user configuration, and sends the relevant parameters and the current neural network to the selfplay server. 

Incoming data files are added to the replay buffer. The sampler uses multiple threads to load training data from those data files, building batches. These batches are used in NN training. Finally newly trained networks are sent to the selfplay server, closing the loop.

The status of the replay buffer and training statistics are plotted in realtime using a Qt GUI.

### Neural network inference framework (Rust)

To run NN inference in the selfplay server, a custom neural network inference framework is used. (see [Scope Creep](https://en.wikipedia.org/wiki/Scope_creep))

This framework is not focused on the specific networks used in AlphaZero (usually ResNet-derived CNNs), but aims to be more general. In particular, it is general enough to support inference of the [Stable Diffusion](https://arxiv.org/abs/2112.10752) and [LLaMA](https://arxiv.org/abs/2302.13971) models.

The typical pipeline is shown in the first figure below. The second figure shows the results of running this pipeline on a simple NN architecture.

![NN inference diagram](./docs/arch_inference.svg)

![conv_bn_sm_flow.svg](./docs/conv_bn_sm_flow.svg)

Central is the _Graph IR_, the intermediate representation for neural network graphs.

The structure is an [SSA](https://en.wikipedia.org/wiki/Static_single-assignment_form)-style directed acyclic graph, where nodes are values with a shape, type and the operation that computes it. These values are abstract, they don't have strides or memory locations yet. The operations are similar to those of other frameworks, but are kept as orthogonal as possible. Some example operations: convolution, matmul, reshape, broadcast, slice, unary, binary, reduce, softmax, ... 

The graph can be constructed directly in code, but for convenience an ONNX loader exists. It can read ONNX files and convert the supported subset of operations into those supported by the IR. Many ONNX operations are decomposed into separate steps, some examples:

* ONNX binary operations implicitly broadcast their operands, but this step is a separate operation in the IR.
* ONNX convolution and matmul have a built-in optional bias operand, this also becomes separate broadcast plus binary addition operation.

The graph can optionally be optimized by the _optimizer_. The optimizations that are currently implemented are:

* Constant folding
* Fusing consecutive affine (bias, scale, batchnorm) operations into a single bias+scale operation.
* Fusing consecutive clamping operations (relu, min, max) into a single min+max operation.
* Strength reduction: replacing division by a constant with multiplication by the inverse constant.
* Recognizing the layernorm template (reduce, subtract, power, reduce, divide) and replacing it with the layernorm operator.

Finally the graph needs to be executed. There is a simple _CPU executor_ that just directly runs each operation. No major optimizations are attempted here, except for using BLAS routines for matmuls and im2col for convolutions. It's important that this executor is as simple as possible because it serves as the baseline for unit tests that check the correctness of the GPU executor.

The second (and more useful) way to run these graphs is with the _Cuda executor_. This involves running the graph though the _Cuda Planner_, which outputs a predetermined schedule of Cuda operations, and allocates the necessary memory buffers. This is split out as a separate step so this expensive planning step only needs to be carried out once per network architecture, the resulting plan can then be reused many times in the executor.

The planner has the following major responsibilities:

* Determine the memory layout of tensors: the strides and the memory offsets
  
  * This implicitly handles reshape, broadcast, stride, ... operations.
  * We also reuse buffers if possible, minimizing total memory usage.

* Decide which cudnn/cublas operations to run for convolutions and matmuls
  
  * If possible, fuse operations together, eg. cudnn supports a "convolution + residual + bias + relu" operation.

* Compile custom kernels for the remaining scalar and compound operations using an _autokernel_ framework based on [NVRTC (Runtime Compilation)](https://docs.nvidia.com/cuda/nvrtc/index.html).
  
  * The operations handled by *autokernel* are: scalar operations, reduce, softmax, layernorm, gather.
  
  * Handwritten kernel templates are used, with details such as tensor shapes, strides, scalar operations, ... substituted in before compilation at runtime.
  
  * More operator fusion happens here
    
    * Multiple scalar operations get compiled to a single kernel
    
    * Constant scalars are inlined
    
    * Some compound kernels support fusing input or output scalar operations

## Current status and results

The AlphaZero implementation fully works. The specific board games that have had training runs are Chess, Ataxx and Go, all relatively successful. These training runs typically last for a couple of days on modest hardware ~2x GTX 3090.

Almost all the code is generic over the specific game being played, so adding new games or even more exotic RL environments should be easy.

MuZero is not working yet, there is some training instability that still has to be fixed.

## File structure

The basic file structure is as follows:

### Python

The `python` folder contains the training code, we use the PyTorch framework. 

* `lib` contains the training framework
* `lib/data` contains data file loading, the replay buffer, the sampler, ...
* `lib/mapping` contains metadata for building the NN architecture for various games
* `lib/lib` contains metadata for building the NN architecture for various games
* `lib/model` contains building blocks for NN architectures
* `main` contains the entry points for starting supervised training or selfplay

### Rust

The `rust` folder is a workspace consisting of a bunch of crates:

#### Core crates

* `kz-core` is the most important crate, it contains the AlphaZero and MuZero tree search implementations, game-specific details and NN wrappers.
* `kz-selfplay` is the selfplay server used during training loops. It runs a bunch of games in parallel on an async thread pool.
* `kz-misc` contains various utilities for evaluating network performance, converting between file formats, and is the crate where I do most of my experimenting in.
* `kz-lichess` is a lichess bot wrapper, you can occasionally play against it here: https://lichess.org/@/kZero-bot

#### Low-level crates

* `cuda-sys` contains Rust wrappers for Cuda and related frameworks, most importantly CuDNN. These wrappers are generated based on the system headers at build time.
* `nn-graph` is a graph format for NN inference, along with an ONNX parser and simple CPU executor.
* `cuda-nn-eval` is a Cuda/CuDNN executor for NN inference, based on the previously mentioned I

#### Utility crates

* `kz-util`: Boring utility functions that don't fit anywhere else.
* `pgn-reader`: A streaming pgn file parser. This was initially useful to pretrain networks on the lichess and CCRL databases.
* `licorice`: A "fork" of https://gitlab.com/hyperchessbotauthor/licorice with some small updates to get it working on lichess again.