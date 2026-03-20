#!/bin/bash

CONTAINER_NAME="cchen_hami1"        
PRIORITY="20"                      # 调度优先级
LOG_PREFIX="inst1"                  # 日志文件前缀
NPU_MEM_QUOTA="28672"

# 路径配置
IMAGE="registry-cbu.huawei.com/ascend/vllm-ascend:v0.10.1rc1"
HOST_PROJECT_DIR="/mnt/nvme0/cchen"  # 宿主机代码/模型路径
CONTAINER_PROJECT_DIR="/cchen"       # 容器内代码路径
HOST_SHARED_DIR="/tmp/hami-shared-region"
GLOBAL_SHM_PATH="/hami-shared-region/global_registry"
LIMITER_PATH="/cchen/hami-vnpu-core/target/debug/limiter"
LIBRARY_PATH="/cchen/hami-vnpu-core/target/debug/libvnpu.so"

# ==========================================
# 2. 宿主机环境预检查
# ==========================================
# echo "--- [Host] 准备环境 ---"
# sudo mkdir -p $HOST_SHARED_DIR
# sudo chmod 777 $HOST_SHARED_DIR

# ==========================================
# 3. 执行启动指令
# ==========================================
echo "--- [Host] 正在拉起容器: $CONTAINER_NAME ---"

# 使用 -d 在后台运行，容器生命周期由 AI 应用进程控制
docker run -d --rm -i --privileged -u root \
    --network=host --pid=host \
    --device=/dev/davinci0 --device=/dev/davinci1 --device=/dev/davinci2 --device=/dev/davinci3 \
    --device=/dev/davinci4 --device=/dev/davinci5 --device=/dev/davinci6 --device=/dev/davinci7 \
    --device=/dev/davinci_manager --device=/dev/devmm_svm --device=/dev/hisi_hdc \
    -v $HOST_SHARED_DIR:/hami-shared-region \
    -v /usr/local/bin/npu-smi:/usr/local/bin/npu-smi \
    -v /etc/ascend_install.info:/etc/ascend_install.info \
    -v /usr/local/Ascend/driver/lib64/driver:/usr/local/Ascend/driver/lib64/driver \
    -v /usr/local/Ascend/driver/version.info:/usr/local/Ascend/driver/version.info \
    -v $HOST_PROJECT_DIR:$CONTAINER_PROJECT_DIR \
    -v /mnt/nvme0/LLMs/:/models \
    --name $CONTAINER_NAME \
    -e NPU_GLOBAL_SHM_PATH=$GLOBAL_SHM_PATH \
    -e NPU_PRIORITY=$PRIORITY \
    -e NPU_MEM_QUOTA=$NPU_MEM_QUOTA \
    -e HOME=/ \
    $IMAGE \
    bash -c "
        cd $CONTAINER_PROJECT_DIR
        export RUST_LOG=info

        # 1. 启动 Manager 到后台，重定向输出到文件
        # echo '[Container] Starting Manager...'
        # ${LIMITER_PATH} > /${CONTAINER_PROJECT_DIR}/${LOG_PREFIX}_manager.log 2>&1 &
        
        # 2. 等待 Manager 完成初始化 (Futex & Shmem)
        # sleep 2
        
        echo '[Container] Starting AI Application with LD_PRELOAD...'
         LD_PRELOAD=${LIBRARY_PATH} python3 /${CONTAINER_PROJECT_DIR}/hami-vnpu-core/test/resnet50.py --prio=${PRIORITY} 2>&1 | tee /${CONTAINER_PROJECT_DIR}/${LOG_PREFIX}_app.log
    "

echo "--- [Host] 容器 $CONTAINER_NAME 已在后台启动 ---"
echo "监控 Manager 日志: tail -f $HOST_PROJECT_DIR/${LOG_PREFIX}_manager.log"
echo "监控 App 日志:      tail -f $HOST_PROJECT_DIR/${LOG_PREFIX}_app.log"