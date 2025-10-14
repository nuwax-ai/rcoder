#!/usr/bin/env node

/**
 * RCoder Request ID 传递机制测试脚本
 * 用于自动化测试 request_id 的正确传递
 */

const axios = require('axios');

// 配置
const RCODER_BASE_URL = 'http://localhost:3000';
const TEST_PROJECT_ID = `test_project_${Date.now()}`;

// 生成测试用的 request_id
function generateRequestId(prefix = 'test') {
  return `${prefix}_${Date.now()}_${Math.random().toString(36).substr(2, 9)}`;
}

// 执行单次测试
async function runSingleTest(testName, customRequestId = null) {
  console.log(`\n🚀 开始测试: ${testName}`);

  const startTime = Date.now();
  const requestId = customRequestId || generateRequestId('auto');

  const testRequest = {
    prompt: `Request ID 测试 - ${testName} - 时间戳: ${Date.now()}`,
    project_id: TEST_PROJECT_ID,
    session_id: `test_session_${Date.now()}`,
    request_id: requestId,
    model_provider: {
      id: "openai_gpt4",
      name: "openai",
      base_url: "https://api.openai.com/v1",
      api_key: "sk-test-key-for-request-id-testing",
      requires_openai_auth: true,
      default_model: "gpt-4",
      api_protocol: "openai"
    }
  };

  try {
    console.log(`📤 发送请求 request_id: ${requestId}`);
    console.log(`📝 项目 ID: ${TEST_PROJECT_ID}`);

    const response = await axios.post(`${RCODER_BASE_URL}/chat`, testRequest, {
      headers: {
        'Content-Type': 'application/json',
      },
      timeout: 30000,
    });

    const endTime = Date.now();
    const responseData = response.data;

    console.log(`📥 收到响应 (${endTime - startTime}ms):`);
    console.log(`   - 响应成功: ${responseData.success}`);

    if (responseData.success && responseData.data) {
      const responseRequestId = responseData.data.request_id;
      console.log(`   - 响应 request_id: ${responseRequestId}`);
      console.log(`   - 项目 ID: ${responseData.data.project_id}`);
      console.log(`   - 会话 ID: ${responseData.data.session_id}`);

      // 验证 request_id 是否匹配
      const isMatch = responseRequestId === requestId;
      console.log(`   - Request ID 匹配: ${isMatch ? '✅ 是' : '❌ 否'}`);

      if (!isMatch) {
        console.log(`⚠️  期望: ${requestId}`);
        console.log(`⚠️  实际: ${responseRequestId}`);
      }

      return {
        success: true,
        testName,
        requestId,
        responseRequestId,
        isMatch,
        duration: endTime - startTime,
        projectId: responseData.data.project_id,
        sessionId: responseData.data.session_id
      };
    } else {
      console.log(`❌ 响应失败: ${responseData.error?.message || '未知错误'}`);
      return {
        success: false,
        testName,
        requestId,
        error: responseData.error?.message || '未知错误',
        duration: endTime - startTime
      };
    }
  } catch (error) {
    const endTime = Date.now();
    console.log(`❌ 请求失败 (${endTime - startTime}ms):`);
    console.log(`   - 错误: ${error.message}`);
    if (error.response) {
      console.log(`   - 状态码: ${error.response.status}`);
      console.log(`   - 响应数据:`, error.response.data);
    }

    return {
      success: false,
      testName,
      requestId,
      error: error.message,
      duration: endTime - startTime
    };
  }
}

// 执行连续测试
async function runSequentialTests() {
  console.log('🎯 开始执行连续测试 (5次请求)...');

  const results = [];
  for (let i = 1; i <= 5; i++) {
    const testName = `连续测试第${i}次`;
    const result = await runSingleTest(testName, `sequential_${i}_${Date.now()}`);
    results.push(result);

    // 等待1秒再进行下一次测试
    if (i < 5) {
      console.log('⏳ 等待1秒后继续下一次测试...');
      await new Promise(resolve => setTimeout(resolve, 1000));
    }
  }

  return results;
}

// 生成测试报告
function generateReport(results) {
  console.log('\n📊 测试报告');
  console.log('=' .repeat(50));

  const totalTests = results.length;
  const successfulTests = results.filter(r => r.success).length;
  const matchedTests = results.filter(r => r.success && r.isMatch).length;
  const failedTests = totalTests - successfulTests;

  console.log(`总测试数: ${totalTests}`);
  console.log(`成功测试: ${successfulTests}`);
  console.log(`Request ID 匹配: ${matchedTests}`);
  console.log(`失败测试: ${failedTests}`);
  console.log(`成功率: ${((successfulTests / totalTests) * 100).toFixed(1)}%`);
  console.log(`匹配率: ${((matchedTests / totalTests) * 100).toFixed(1)}%`);

  if (successfulTests > 0) {
    const durations = results.filter(r => r.success).map(r => r.duration);
    const avgDuration = durations.reduce((a, b) => a + b, 0) / durations.length;
    console.log(`平均响应时间: ${avgDuration.toFixed(0)}ms`);
  }

  console.log('\n📋 详细结果:');
  results.forEach((result, index) => {
    console.log(`${index + 1}. ${result.testName}`);
    if (result.success) {
      console.log(`   ✅ 成功 - 匹配: ${result.isMatch ? '是' : '否'} - ${result.duration}ms`);
      console.log(`   📤 请求: ${result.requestId}`);
      console.log(`   📥 响应: ${result.responseRequestId}`);
    } else {
      console.log(`   ❌ 失败 - ${result.error} - ${result.duration}ms`);
      console.log(`   📤 请求: ${result.requestId}`);
    }
  });

  return {
    totalTests,
    successfulTests,
    matchedTests,
    failedTests,
    successRate: (successfulTests / totalTests) * 100,
    matchRate: (matchedTests / totalTests) * 100
  };
}

// 主函数
async function main() {
  console.log('🧪 RCoder Request ID 传递机制测试');
  console.log('=' .repeat(50));
  console.log(`📍 测试目标: ${RCODER_BASE_URL}`);
  console.log(`📁 测试项目: ${TEST_PROJECT_ID}`);
  console.log(`⏰ 开始时间: ${new Date().toISOString()}`);

  try {
    // 检查服务是否可用
    console.log('\n🔍 检查 RCoder 服务是否可用...');
    await axios.get(`${RCODER_BASE_URL}/`, { timeout: 5000 });
    console.log('✅ RCoder 服务可用');

    // 执行测试
    console.log('\n🎯 开始执行测试...');

    // 测试1: 无自定义 request_id（让系统自动生成）
    const result1 = await runSingleTest('自动生成 request_id');

    // 测试2: 自定义 request_id
    const result2 = await runSingleTest('自定义 request_id', `custom_test_${Date.now()}`);

    // 测试3: 特殊字符 request_id
    const result3 = await runSingleTest('特殊字符 request_id', `special-test_123_ABC_${Date.now()}`);

    // 测试4: 空 request_id (应该被系统自动生成)
    const result4 = await runSingleTest('空 request_id', '');

    // 执行连续测试
    const sequentialResults = await runSequentialTests();

    // 生成综合报告
    const allResults = [result1, result2, result3, result4, ...sequentialResults];
    const report = generateReport(allResults);

    console.log('\n🏁 测试完成');
    console.log(`📊 最终结果: ${report.matchRate.toFixed(1)}% 的测试 request_id 正确匹配`);

    if (report.matchRate >= 90) {
      console.log('🎉 Request ID 传递机制测试通过！');
      process.exit(0);
    } else {
      console.log('⚠️  Request ID 传递机制存在问题，需要进一步检查');
      process.exit(1);
    }

  } catch (error) {
    console.error('\n💥 测试过程中发生错误:');
    console.error(error.message);

    if (error.code === 'ECONNREFUSED') {
      console.error('❌ 无法连接到 RCoder 服务，请确保服务正在运行');
      console.error(`   服务地址: ${RCODER_BASE_URL}`);
    }

    process.exit(1);
  }
}

// 运行测试
if (require.main === module) {
  main().catch(console.error);
}

module.exports = {
  runSingleTest,
  runSequentialTests,
  generateReport
};