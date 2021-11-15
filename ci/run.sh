#!/bin/bash
SCRIPT_DIR=$(cd $(dirname $0); pwd)
export VK_ICD_FILENAMES="${SCRIPT_DIR}/mesa/share/vulkan/icd.d/radeon_icd.x86_64.json"
"${SCRIPT_DIR}/acominer" "$@"
